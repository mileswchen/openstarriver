//! Lossless row packing for `assistant/chunk` delta runs. Providers stream
//! token-sized deltas, so the log stores thousands of near-identical event
//! rows whose JSON envelopes dwarf their payloads (~56x measured on a real
//! DeepSeek session in; 2026-09-02 game002 measured 41,686 rows in one
//! 12-minute turn). This module packs each run of consecutive same-block
//! delta chunks into ONE storage row — `text-chunks`, `reasoning-chunks`, or
//! `tool-call-chunks` — and expands rows back to the exact original events.
//!
//! Packed rows are an encoding vocabulary, NOT session events: they never
//! enter `Session.events` and use bare (slash-less) type tags in the events
//! table's `type` column, so a reader cannot confuse them with the event
//! taxonomy (same discipline as JSONL header `session` tag). The
//! encoder whitelists exact shapes — anything it does not fully recognize
//! stays verbatim, so unknown fields or future chunk variants lose
//! compression, never data. The decoder validates before expanding and
//! fails loud on a malformed row instead of silently dropping a whole run.
//! Port of `core/session/chunk-rows.ts`.

use starriver_kernel::json::JsonValue;
use starriver_kernel::session::SessionEvent;

/// Minimum members before a run packs. Below it a row's envelope rivals the
/// event lines it replaces. A format constant, not a tunable: both layouts
/// decode identically, so changing it never invalidates stored logs.
pub const MIN_RUN: usize = 3;

const TEXT_CHUNKS: &str = "text-chunks";
const REASONING_CHUNKS: &str = "reasoning-chunks";
const TOOL_CALL_CHUNKS: &str = "tool-call-chunks";

/// Whether a stored `type` value denotes a packed chunk row (rather than a
/// session event). Slash-less by construction; the event taxonomy is
/// slash-namespaced, so the vocabularies cannot collide.
pub fn is_chunk_row_tag(tag: &str) -> bool {
    matches!(tag, TEXT_CHUNKS | REASONING_CHUNKS | TOOL_CALL_CHUNKS)
}

/// The chunk kinds that may pack; block boundaries, usage, and finish
/// chunks always stay one event per row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
    ToolCall,
}

impl DeltaKind {
    /// The bare storage tag this packed kind serializes under.
    pub fn row_tag(self) -> &'static str {
        match self {
            DeltaKind::Text => TEXT_CHUNKS,
            DeltaKind::Reasoning => REASONING_CHUNKS,
            DeltaKind::ToolCall => TOOL_CALL_CHUNKS,
        }
    }
}

/// One durable row's worth of data: a verbatim event or a packed run.
pub enum StorageRecord {
    Event(SessionEvent),
    ChunkRow {
        seq0: u64,
        time0: u64,
        kind: DeltaKind,
        data: RunData,
    },
}

/// A packed run's payload: placement, block correlation, member timestamps
/// as gaps, and the per-member texts/arguments (never joined — token
/// boundaries are data).
pub enum RunData {
    Text {
        turn: u64,
        step: u64,
        index: u64,
        dt: Vec<i64>,
        texts: Vec<String>,
    },
    ToolCall {
        turn: u64,
        step: u64,
        index: u64,
        id: String,
        /// Present iff every member carried it, with one uniform value (a
        /// mixed run never packs).
        name: Option<String>,
        dt: Vec<i64>,
        args: Vec<String>,
    },
}

impl RunData {
    /// Member count of the run.
    pub fn len(&self) -> usize {
        match self {
            RunData::Text { texts, .. } => texts.len(),
            RunData::ToolCall { args, .. } => args.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn to_json(&self) -> JsonValue {
        let uint = |v: u64| JsonValue::UInt(v);
        let gaps = |dt: &[i64]| JsonValue::Array(dt.iter().map(|g| JsonValue::Int(*g)).collect());
        match self {
            RunData::Text {
                turn,
                step,
                index,
                dt,
                texts,
            } => JsonValue::Object(vec![
                ("turn".into(), uint(*turn)),
                ("step".into(), uint(*step)),
                ("index".into(), uint(*index)),
                ("dt".into(), gaps(dt)),
                (
                    "texts".into(),
                    JsonValue::Array(texts.iter().map(|t| JsonValue::String(t.clone())).collect()),
                ),
            ]),
            RunData::ToolCall {
                turn,
                step,
                index,
                id,
                name,
                dt,
                args,
            } => {
                let mut fields = vec![
                    ("turn".into(), uint(*turn)),
                    ("step".into(), uint(*step)),
                    ("index".into(), uint(*index)),
                    ("id".into(), JsonValue::String(id.clone())),
                ];
                if let Some(name) = name {
                    fields.push(("name".into(), JsonValue::String(name.clone())));
                }
                fields.push(("dt".into(), gaps(dt)));
                fields.push((
                    "args".into(),
                    JsonValue::Array(args.iter().map(|a| JsonValue::String(a.clone())).collect()),
                ));
                JsonValue::Object(fields)
            }
        }
    }
}

/// A whitelisted delta chunk's classification: enough borrowed detail to
/// decide run continuation without re-parsing.
struct Delta<'a> {
    kind: DeltaKind,
    turn: u64,
    step: u64,
    index: u64,
    /// Tool-call identity (tool-call deltas only).
    id: Option<&'a str>,
    name: Option<&'a str>,
}

/// Exact-key check: `object` has every key in `keys` and nothing else.
fn has_exact_keys(object: &[(String, JsonValue)], keys: &[&str]) -> bool {
    object.len() == keys.len() && keys.iter().all(|k| object.iter().any(|(f, _)| f == k))
}

fn object_fields(value: &JsonValue) -> Option<&[(String, JsonValue)]> {
    match value {
        JsonValue::Object(fields) => Some(fields),
        _ => None,
    }
}

/// Classify an event for packing: its delta kind when the ENTIRE shape
/// (envelope, data, chunk — exact keys, primitive types, no surface
/// metadata) is whitelisted, else `None` (store verbatim).
fn classify(event: &SessionEvent) -> Option<Delta<'_>> {
    if event.event_type != "assistant/chunk" {
        return None;
    }
    if event.surface_op.is_some() || !event.source_event_seqs.is_empty() {
        return None;
    }
    let fields = object_fields(&event.data)?;
    if !has_exact_keys(fields, &["turn", "step", "chunk"]) {
        return None;
    }
    let turn = event.data.get("turn")?.as_u64()?;
    let step = event.data.get("step")?.as_u64()?;
    let chunk = event.data.get("chunk")?;
    let chunk_fields = object_fields(chunk)?;
    let chunk_type = chunk.get("type")?.as_str()?;
    let index = chunk.get("index")?.as_u64()?;
    match chunk_type {
        "text-delta" | "reasoning-delta" => {
            if !has_exact_keys(chunk_fields, &["type", "index", "text"]) {
                return None;
            }
            chunk.get("text")?.as_str()?;
            Some(Delta {
                kind: if chunk_type == "text-delta" {
                    DeltaKind::Text
                } else {
                    DeltaKind::Reasoning
                },
                turn,
                step,
                index,
                id: None,
                name: None,
            })
        }
        "tool-call-delta" => {
            let with_name = has_exact_keys(
                chunk_fields,
                &["type", "index", "id", "name", "argumentsDelta"],
            );
            let without_name =
                has_exact_keys(chunk_fields, &["type", "index", "id", "argumentsDelta"]);
            if !with_name && !without_name {
                return None;
            }
            let id = chunk.get("id")?.as_str()?;
            let name = if with_name {
                Some(chunk.get("name")?.as_str()?)
            } else {
                None
            };
            chunk.get("argumentsDelta")?.as_str()?;
            Some(Delta {
                kind: DeltaKind::ToolCall,
                turn,
                step,
                index,
                id: Some(id),
                name,
            })
        }
        // Whitelist fall-through over parsed data: block-start/end, usage,
        // finish, and any future chunk variant stay one event per row.
        _ => None,
    }
}

/// Whether `next` extends a run ending in `prev` (same kind already checked
/// by the caller).
fn continues(prev: (&SessionEvent, &Delta<'_>), next: (&SessionEvent, &Delta<'_>)) -> bool {
    if next.0.seq != prev.0.seq + 1 {
        return false;
    }
    if next.1.turn != prev.1.turn || next.1.step != prev.1.step || next.1.index != prev.1.index {
        return false;
    }
    if prev.1.kind != DeltaKind::ToolCall {
        return true;
    }
    // `name` must match in presence AND value — a mixed run is not
    // representable.
    prev.1.id == next.1.id && prev.1.name == next.1.name
}

fn member_text(event: &SessionEvent) -> String {
    event
        .data
        .get("chunk")
        .and_then(|c| c.get("text"))
        .and_then(JsonValue::as_str)
        .expect("classify whitelisted a text payload")
        .to_string()
}

fn member_args(event: &SessionEvent) -> String {
    event
        .data
        .get("chunk")
        .and_then(|c| c.get("argumentsDelta"))
        .and_then(JsonValue::as_str)
        .expect("classify whitelisted an arguments payload")
        .to_string()
}

/// Build the row for a completed run (`run.len() >= MIN_RUN`, uniform per
/// `continues`).
fn build_row(run: &[(&SessionEvent, Delta<'_>)]) -> StorageRecord {
    let (first, first_delta) = (&run[0].0, &run[0].1);
    let dt = run
        .windows(2)
        .map(|pair| {
            // Times are epoch-ms u64s; the gap may be negative when the wall
            // clock stepped backwards between events.
            (pair[1].0.time as i64) - (pair[0].0.time as i64)
        })
        .collect::<Vec<_>>();
    let payload = match first_delta.kind {
        DeltaKind::ToolCall => RunData::ToolCall {
            turn: first_delta.turn,
            step: first_delta.step,
            index: first_delta.index,
            id: first_delta
                .id
                .expect("tool-call deltas carry id")
                .to_string(),
            name: first_delta.name.map(str::to_string),
            dt,
            args: run.iter().map(|(e, _)| member_args(e)).collect(),
        },
        DeltaKind::Text | DeltaKind::Reasoning => RunData::Text {
            turn: first_delta.turn,
            step: first_delta.step,
            index: first_delta.index,
            dt,
            texts: run.iter().map(|(e, _)| member_text(e)).collect(),
        },
    };
    StorageRecord::ChunkRow {
        seq0: first.seq,
        time0: first.time,
        kind: first_delta.kind,
        data: payload,
    }
}

/// Pack an event batch for storage: each run of at least `MIN_RUN`
/// consecutive whitelisted same-kind, same-block delta chunk events becomes
/// one packed row; every other event passes through verbatim, in order.
/// Pure and stateless — safe over any array, including a batch whose runs
/// were split by flush boundaries (the split runs simply pack per batch).
pub fn pack_chunk_runs(events: &[SessionEvent]) -> Vec<StorageRecord> {
    let mut out = Vec::new();
    let mut run: Vec<(&SessionEvent, Delta<'_>)> = Vec::new();
    let mut kind: Option<DeltaKind> = None;
    fn flush(run: &mut Vec<(&SessionEvent, Delta<'_>)>, out: &mut Vec<StorageRecord>) {
        if run.len() >= MIN_RUN {
            out.push(build_row(run));
        } else {
            out.extend(
                run.iter()
                    .map(|(event, _)| StorageRecord::Event((*event).clone())),
            );
        }
        run.clear();
    }
    for event in events {
        match classify(event) {
            None => {
                flush(&mut run, &mut out);
                kind = None;
                out.push(StorageRecord::Event(event.clone()));
            }
            Some(delta) => {
                let extends = kind == Some(delta.kind)
                    && run
                        .last()
                        .is_some_and(|prev| continues((prev.0, &prev.1), (event, &delta)));
                if !extends {
                    flush(&mut run, &mut out);
                    kind = Some(delta.kind);
                }
                run.push((event, delta));
            }
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Serialize one packed row's `data` payload for the events table.
pub fn chunk_row_data_json(data: &RunData) -> Result<String, String> {
    data.to_json()
        .to_json_string()
        .map_err(|e| format!("packed row data serialization failed: {e}"))
}

fn malformed(tag: &str, why: impl Into<String>) -> String {
    format!("malformed {tag} storage row: {}", why.into())
}

fn u64_of(value: &JsonValue) -> Option<u64> {
    value.as_u64()
}

/// Validate a row-tagged parsed payload and expand it back into its exact
/// original events, in order. Any malformation is an error — it is corrupt
/// storage, and treating it as an event would silently drop a whole run.
pub fn decode_chunk_row(
    tag: &str,
    seq0: u64,
    time0: u64,
    data: &JsonValue,
) -> Result<Vec<SessionEvent>, String> {
    let fields = object_fields(data).ok_or_else(|| malformed(tag, "data must be an object"))?;
    let turn = u64_of(
        data.get("turn")
            .ok_or_else(|| malformed(tag, "missing turn"))?,
    )
    .ok_or_else(|| malformed(tag, "turn must be a non-negative integer"))?;
    let step = u64_of(
        data.get("step")
            .ok_or_else(|| malformed(tag, "missing step"))?,
    )
    .ok_or_else(|| malformed(tag, "step must be a non-negative integer"))?;
    let index = u64_of(
        data.get("index")
            .ok_or_else(|| malformed(tag, "missing index"))?,
    )
    .ok_or_else(|| malformed(tag, "index must be a non-negative integer"))?;
    let gaps_of = |data: &JsonValue| -> Option<Vec<i64>> {
        data.get("dt")?.as_array().map(|entries| {
            entries
                .iter()
                .map(|g| match g {
                    JsonValue::Int(v) => Some(*v),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
        })?
    };
    let (id, name, payload): (Option<String>, Option<String>, Vec<String>) = match tag {
        TOOL_CALL_CHUNKS => {
            if !has_exact_keys(
                fields,
                &["turn", "step", "index", "id", "name", "dt", "args"],
            ) && !has_exact_keys(fields, &["turn", "step", "index", "id", "dt", "args"])
            {
                return Err(malformed(
                    tag,
                    "data must be exactly {turn, step, index, id, name?, dt, args}",
                ));
            }
            let id = data
                .get("id")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| malformed(tag, "id must be a string"))?
                .to_string();
            let name = match data.get("name") {
                None => None,
                Some(value) => Some(
                    value
                        .as_str()
                        .ok_or_else(|| malformed(tag, "name must be a string"))?
                        .to_string(),
                ),
            };
            let args = string_array(
                data.get("args")
                    .ok_or_else(|| malformed(tag, "missing args"))?,
                tag,
                "args",
            )?;
            (Some(id), name, args)
        }
        TEXT_CHUNKS | REASONING_CHUNKS => {
            if !has_exact_keys(fields, &["turn", "step", "index", "dt", "texts"]) {
                return Err(malformed(
                    tag,
                    "data must be exactly {turn, step, index, dt, texts}",
                ));
            }
            let texts = string_array(
                data.get("texts")
                    .ok_or_else(|| malformed(tag, "missing texts"))?,
                tag,
                "texts",
            )?;
            (None, None, texts)
        }
        _ => return Err(malformed(tag, "unknown packed row tag")),
    };
    let dt = gaps_of(data).ok_or_else(|| malformed(tag, "dt must be an array of integers"))?;
    if dt.len() != payload.len() - 1 {
        return Err(malformed(
            tag,
            format!(
                "dt length {} does not match {} members",
                dt.len(),
                payload.len()
            ),
        ));
    }
    // Reconstruction bounds: member seqs and times must stay representable.
    // Within range every step is exact, so the first departure is caught.
    if payload.len() as u64 > u64::MAX - seq0 {
        return Err(malformed(tag, "member seqs must stay addressable"));
    }
    let mut time = time0 as i64;
    for gap in &dt {
        time = time
            .checked_add(*gap)
            .ok_or_else(|| malformed(tag, "member times overflow"))?;
        if time < 0 {
            return Err(malformed(tag, "member times must stay non-negative"));
        }
    }
    let kind = match tag {
        TEXT_CHUNKS => DeltaKind::Text,
        REASONING_CHUNKS => DeltaKind::Reasoning,
        TOOL_CALL_CHUNKS => DeltaKind::ToolCall,
        _ => unreachable!("tag validated above"),
    };
    let mut events = Vec::with_capacity(payload.len());
    let mut time = time0 as i64;
    for (k, member) in payload.iter().enumerate() {
        if k > 0 {
            time += dt[k - 1];
        }
        let chunk = match (&kind, &id) {
            (DeltaKind::Text, _) => JsonValue::Object(vec![
                ("type".into(), JsonValue::String("text-delta".into())),
                ("index".into(), JsonValue::UInt(index)),
                ("text".into(), JsonValue::String(member.clone())),
            ]),
            (DeltaKind::Reasoning, _) => JsonValue::Object(vec![
                ("type".into(), JsonValue::String("reasoning-delta".into())),
                ("index".into(), JsonValue::UInt(index)),
                ("text".into(), JsonValue::String(member.clone())),
            ]),
            (DeltaKind::ToolCall, Some(id)) => {
                let mut fields = vec![
                    ("type".into(), JsonValue::String("tool-call-delta".into())),
                    ("index".into(), JsonValue::UInt(index)),
                    ("id".into(), JsonValue::String(id.clone())),
                ];
                if let Some(name) = &name {
                    fields.push(("name".into(), JsonValue::String(name.clone())));
                }
                fields.push(("argumentsDelta".into(), JsonValue::String(member.clone())));
                JsonValue::Object(fields)
            }
            (DeltaKind::ToolCall, None) => {
                return Err(malformed(tag, "tool-call row missing id"));
            }
        };
        events.push(SessionEvent {
            seq: seq0 + k as u64,
            time: time as u64,
            event_type: "assistant/chunk".into(),
            data: JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(turn)),
                ("step".into(), JsonValue::UInt(step)),
                ("chunk".into(), chunk),
            ]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        });
    }
    Ok(events)
}

fn string_array(value: &JsonValue, tag: &str, key: &str) -> Result<Vec<String>, String> {
    let entries = value
        .as_array()
        .ok_or_else(|| malformed(tag, format!("{key} must be a non-empty string array")))?;
    if entries.is_empty() {
        return Err(malformed(
            tag,
            format!("{key} must be a non-empty string array"),
        ));
    }
    entries
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| malformed(tag, format!("{key} must be a non-empty string array")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta_event(seq: u64, time: u64, chunk: JsonValue) -> SessionEvent {
        SessionEvent {
            seq,
            time,
            event_type: "assistant/chunk".into(),
            data: JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(1)),
                ("step".into(), JsonValue::UInt(2)),
                ("chunk".into(), chunk),
            ]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        }
    }

    fn text_delta(seq: u64, time: u64, text: &str) -> SessionEvent {
        delta_event(
            seq,
            time,
            JsonValue::Object(vec![
                ("type".into(), JsonValue::String("text-delta".into())),
                ("index".into(), JsonValue::UInt(0)),
                ("text".into(), JsonValue::String(text.into())),
            ]),
        )
    }

    fn reasoning_delta(seq: u64, time: u64, text: &str) -> SessionEvent {
        delta_event(
            seq,
            time,
            JsonValue::Object(vec![
                ("type".into(), JsonValue::String("reasoning-delta".into())),
                ("index".into(), JsonValue::UInt(1)),
                ("text".into(), JsonValue::String(text.into())),
            ]),
        )
    }

    fn tool_delta(seq: u64, time: u64, args: &str) -> SessionEvent {
        delta_event(
            seq,
            time,
            JsonValue::Object(vec![
                ("type".into(), JsonValue::String("tool-call-delta".into())),
                ("index".into(), JsonValue::UInt(2)),
                ("id".into(), JsonValue::String("call-1".into())),
                ("name".into(), JsonValue::String("bash".into())),
                ("argumentsDelta".into(), JsonValue::String(args.into())),
            ]),
        )
    }

    fn block_end(seq: u64, time: u64) -> SessionEvent {
        delta_event(
            seq,
            time,
            JsonValue::Object(vec![
                ("type".into(), JsonValue::String("block-end".into())),
                ("index".into(), JsonValue::UInt(0)),
            ]),
        )
    }

    #[test]
    fn pack_and_decode_round_trip_every_kind() {
        let events = vec![
            text_delta(0, 100, "a"),
            text_delta(1, 110, "b"),
            text_delta(2, 125, "c"),
            text_delta(3, 130, "d"),
            block_end(4, 131),
            reasoning_delta(5, 140, "t"),
            reasoning_delta(6, 141, "h"),
            reasoning_delta(7, 150, "k"),
            tool_delta(8, 160, "{\""),
            tool_delta(9, 161, "cmd"),
            tool_delta(10, 165, "\"}"),
        ];
        let records = pack_chunk_runs(&events);
        // Four packed rows (text run, reasoning run, tool run) plus the
        // verbatim block-end; the two runs of length >= 3 pack.
        assert_eq!(records.len(), 4, "3 packed runs + 1 verbatim block-end");
        let decoded = records
            .iter()
            .map(|record| match record {
                StorageRecord::Event(event) => vec![event.clone()],
                StorageRecord::ChunkRow {
                    seq0,
                    time0,
                    kind,
                    data,
                } => {
                    let tag = kind.row_tag();
                    decode_chunk_row(tag, *seq0, *time0, &data.to_json()).unwrap()
                }
            })
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(decoded.len(), events.len());
        for (got, want) in decoded.iter().zip(&events) {
            assert_eq!(got.seq, want.seq);
            assert_eq!(got.time, want.time);
            assert_eq!(got.event_type, want.event_type);
            assert_eq!(
                got.data.to_json_string().unwrap(),
                want.data.to_json_string().unwrap(),
                "member {got:?} must round-trip byte-identically",
            );
        }
    }

    #[test]
    fn short_runs_and_unknown_shapes_stay_verbatim() {
        let mut events = vec![text_delta(0, 10, "x"), text_delta(1, 11, "y")];
        // A chunk with an unknown future shape never packs.
        events.push(delta_event(
            2,
            12,
            JsonValue::Object(vec![
                ("type".into(), JsonValue::String("future-delta".into())),
                ("index".into(), JsonValue::UInt(0)),
            ]),
        ));
        // A surface-op-carrying chunk never packs.
        let mut surface = text_delta(3, 13, "z");
        surface.surface_op = Some(starriver_kernel::session::SurfaceOp::Append);
        events.push(surface);
        let records = pack_chunk_runs(&events);
        assert!(records
            .iter()
            .all(|record| matches!(record, StorageRecord::Event(_))));
    }

    #[test]
    fn run_breaks_on_block_step_or_identity_change() {
        // Different block index breaks the run.
        let mut events = vec![
            text_delta(0, 10, "a"),
            text_delta(1, 11, "b"),
            text_delta(2, 12, "c"),
        ];
        let mut other_block = text_delta(3, 13, "d");
        other_block.data = JsonValue::Object(vec![
            ("turn".into(), JsonValue::UInt(1)),
            ("step".into(), JsonValue::UInt(2)),
            (
                "chunk".into(),
                JsonValue::Object(vec![
                    ("type".into(), JsonValue::String("text-delta".into())),
                    ("index".into(), JsonValue::UInt(7)),
                    ("text".into(), JsonValue::String("d".into())),
                ]),
            ),
        ]);
        events.push(other_block);
        events.push(text_delta(4, 14, "e"));
        events.push(text_delta(5, 15, "f"));
        events.push(text_delta(6, 16, "g"));
        let records = pack_chunk_runs(&events);
        // Two packed 3-runs plus the index-7 stranger between them, verbatim.
        assert_eq!(records.len(), 3);
        assert!(matches!(records[1], StorageRecord::Event(_)));
        // Non-consecutive seq breaks the run even for same-block deltas.
        let gapped = text_delta(7, 20, "g");
        let records = pack_chunk_runs(&[text_delta(5, 15, "e"), gapped]);
        assert!(records
            .iter()
            .all(|record| matches!(record, StorageRecord::Event(_))));
    }

    #[test]
    fn negative_time_gaps_round_trip() {
        // Wall clock stepped backwards between members.
        let events = vec![
            text_delta(0, 200, "a"),
            text_delta(1, 190, "b"),
            text_delta(2, 195, "c"),
        ];
        let records = pack_chunk_runs(&events);
        let StorageRecord::ChunkRow {
            seq0,
            time0,
            kind,
            data,
        } = &records[0]
        else {
            panic!("expected a packed row");
        };
        let decoded = decode_chunk_row(kind.row_tag(), *seq0, *time0, &data.to_json()).unwrap();
        assert_eq!(decoded[1].time, 190);
        assert_eq!(decoded[2].time, 195);
    }

    #[test]
    fn decode_rejects_malformed_rows() {
        let records = pack_chunk_runs(&[
            text_delta(0, 100, "a"),
            text_delta(1, 110, "b"),
            text_delta(2, 125, "c"),
        ]);
        let StorageRecord::ChunkRow {
            seq0,
            time0,
            kind,
            data,
        } = &records[0]
        else {
            panic!("expected a packed row");
        };
        let tag = kind.row_tag();
        let json = data.to_json();
        // dt arity mismatch.
        let mut bad_dt = json.clone();
        bad_dt = match bad_dt {
            JsonValue::Object(mut fields) => {
                fields.retain(|(k, _)| k != "dt");
                JsonValue::Object(fields)
            }
            other => other,
        };
        assert!(decode_chunk_row(tag, *seq0, *time0, &bad_dt).is_err());
        // Non-string member.
        let mut bad_texts = json.clone();
        bad_texts = match bad_texts {
            JsonValue::Object(mut fields) => {
                for (k, v) in fields.iter_mut() {
                    if k == "texts" {
                        *v = JsonValue::Array(vec![JsonValue::UInt(1)]);
                    }
                }
                JsonValue::Object(fields)
            }
            other => other,
        };
        assert!(decode_chunk_row(tag, *seq0, *time0, &bad_texts).is_err());
        // Extra unknown key.
        let extra = match json.clone() {
            JsonValue::Object(mut fields) => {
                fields.push(("extra".into(), JsonValue::Null));
                JsonValue::Object(fields)
            }
            other => other,
        };
        assert!(decode_chunk_row(tag, *seq0, *time0, &extra).is_err());
        // Negative member time underflow.
        let bad_time = match json {
            JsonValue::Object(mut fields) => {
                for (k, v) in fields.iter_mut() {
                    if k == "dt" {
                        *v = JsonValue::Array(vec![JsonValue::Int(-1000), JsonValue::Int(0)]);
                    }
                }
                JsonValue::Object(fields)
            }
            other => other,
        };
        assert!(decode_chunk_row(tag, *seq0, 101, &bad_time).is_err());
    }
}
