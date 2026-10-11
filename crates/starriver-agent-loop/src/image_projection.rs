//! WI-1/WI-159: image cold/hot projection (`maxImagesPerRequest` /
//! `maxRequestFilesBytes` budget parity — byte-budgeted, no fixed window).
//! The session log stores every image as a `sha256:` reference; this engine
//! runs at request-assembly time — after `derive_messages`, before
//! `GenerateOptions` — and rewrites the projected messages so only the
//! images that fit the request budget ride the wire as data URLs. Cold ones
//! become metadata placeholder text the model can act on via
//! `reload_image` (WI-152 S3: no content summaries — identity + recovery
//! path only).
//!
//! Budget semantics (WI-159): every image is a hot candidate — small images
//! pack many per request, large ones few. When either the count cap or the
//! byte budget (base64 wire length) is exceeded, the OLDEST slots shed
//! first; a `reload_image` pull this turn pins its slot ahead of unpinned
//! slots, and the newest slot is always kept (a single oversized image
//! still rides). A pinned slot displaced by the budget gets a placeholder
//! that states the pull already happened instead of inviting another call
//! (BUG-114: a silently-revoked reload plus a standing re-call hint
//! livelocked the model into dozens of repeat calls). The outcome depends
//! only on the durable surface and this turn's reload set, so it never
//! flips between steps without cause.

use std::collections::HashSet;

use starriver_kernel::agent::{ImageProjectionConfig, ImageResolver};
use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{ContentBlock, Message};

/// The tool that pulls a historical image back into the hot set for the
/// current turn (host-side definition: `crate::tools::reload_image`).
pub const RELOAD_IMAGE_TOOL: &str = "reload_image";

/// One image occurrence on the projected surface, in surface order.
struct ImageSlot {
    message: usize,
    block: usize,
    /// `Some(i)`: the Image is `content[i]` nested inside the ToolResult at
    /// `(message, block)` — BUG-119: nested tool-result images join the
 /// budget and the cold/hot projection like any other occurrence (
    /// `collectImageLengths` parity).
    inner: Option<usize>,
    /// `sha256:<hex>` reference. `None` = legacy inline data URL (pre-WI-1
    /// log content): still a hot candidate, but unreloadable.
    file_id: Option<String>,
    /// Base64 wire length if knowable — fileId slots derive it from the
    /// resolver's payload byte length, inline slots from the data URL's own
    /// length. `None` = unknown (the slot only counts against the count
    /// cap). Budget accounting uses wire length because that is what
    /// actually rides the request.
    wire_len: Option<u64>,
}

/// Extract the `file_id` argument from a `reload_image` call's lossless
/// arguments (WI-47 Phase B: the field is a JsonValue now). Non-object or
/// missing/blank ids yield `None`.
pub fn parse_reload_file_id(arguments: &JsonValue) -> Option<String> {
    let id = arguments.get("file_id")?.as_str()?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// BUG-118 fallback: a host that configured image projection but failed to
/// bridge a resolver must not silently disable the engine — fileId Image
/// blocks would then ride the wire raw (most providers reject them). This
/// stub resolves nothing, so every fileId slot degrades to its cold
/// placeholder while inline slots still budget normally.
pub struct MissingResolver;

impl ImageResolver for MissingResolver {
    fn resolve(&self, _file_id: &str) -> Option<String> {
        None
    }
}

/// Base64 length of `bytes` raw payload, including padding (
/// `base64Length`).
fn base64_wire_len(bytes: u64) -> u64 {
    (bytes.div_ceil(3)) * 4
}

fn image_slots(messages: &[Message], resolver: &dyn ImageResolver) -> Vec<ImageSlot> {
    let mut slots = Vec::new();
    let describe = |attachment: &JsonValue| {
        let file_id = attachment
            .as_str_at("fileId")
            .map(str::to_string)
            .filter(|id| id.starts_with("sha256:"));
        let wire_len = match &file_id {
            Some(file_id) => resolver.byte_len(file_id).map(base64_wire_len),
            // Legacy inline slot: budget the base64 payload only — the
            // `data:<media>;base64,` prefix is framing, not image bytes
 // (`base64Length` parity; the prefix used to inflate every
            // inline slot's cost).
            None => attachment
                .as_str_at("url")
                .and_then(|url| url.split_once(','))
                .map(|(_, payload)| payload.len() as u64),
        };
        (file_id, wire_len)
    };
    for (mi, message) in messages.iter().enumerate() {
        for (bi, block) in message.content.iter().enumerate() {
            match block {
                ContentBlock::Image { attachment } => {
                    let (file_id, wire_len) = describe(attachment);
                    slots.push(ImageSlot {
                        message: mi,
                        block: bi,
                        inner: None,
                        file_id,
                        wire_len,
                    });
                }
                // BUG-119: tool results may embed images (screenshot-style
                // tools); they ride the same budget — no free riders.
                ContentBlock::ToolResult { content, .. } => {
                    for (ii, nested) in content.iter().enumerate() {
                        let ContentBlock::Image { attachment } = nested else {
                            continue;
                        };
                        let (file_id, wire_len) = describe(attachment);
                        slots.push(ImageSlot {
                            message: mi,
                            block: bi,
                            inner: Some(ii),
                            file_id,
                            wire_len,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    slots
}

/// Swap one slot's block in place, addressing the nested tool-result form.
fn replace_slot(messages: &mut [Message], slot: &ImageSlot, block: ContentBlock) {
    let outer = &mut messages[slot.message].content[slot.block];
    match slot.inner {
        Some(index) => {
            if let ContentBlock::ToolResult { content, .. } = outer {
                content[index] = block;
            }
        }
        None => *outer = block,
    }
}

/// How a cold placeholder may talk about recovery.
#[derive(Clone, Copy)]
enum ColdHint {
    /// Vision run: the reload recovery path is real.
    Reload,
    /// WI-152 S2: the model accepts text only.
    TextOnly,
    /// WI-160: one budget dimension is configured to 0 — the run carries no
    /// image data by explicit (possibly degenerate) choice; a reload pull
    /// could never ride, so the placeholder must not invite one.
    BudgetDisabled,
}

/// Rewrite the projected surface in place: the images that fit the request
/// budget (count cap + byte budget, reload pins prioritized) are resolved
/// to data URLs; the rest become metadata placeholder text. The newest
/// slot always stays hot unless the run is image-disabled.
pub fn project_images(
    messages: &mut [Message],
    cfg: &ImageProjectionConfig,
    resolver: &dyn ImageResolver,
    reload_ids: &HashSet<String>,
) {
    let slots = image_slots(messages, resolver);
    if slots.is_empty() {
        return;
    }
    // A zero in EITHER budget dimension disables images entirely — not just
    // the (0, 0) text-only stamp. A single-zero config (say images=8,
    // bytes=0) must not keep the newest slot riding via the floor nor
    // advertise a reload that could never take effect (WI-160).
    let disabled = cfg.max_images == 0 || cfg.max_request_bytes == 0;
    let hint = if !cfg.allow_reload {
        ColdHint::TextOnly
    } else if disabled {
        ColdHint::BudgetDisabled
    } else {
        ColdHint::Reload
    };
    let pinned = |slot: &ImageSlot| {
        !disabled
            && slot
                .file_id
                .as_deref()
                .is_some_and(|id| cfg.allow_reload && reload_ids.contains(id))
    };
    let mut hot: HashSet<usize> = if disabled {
        HashSet::new()
    } else {
        (0..slots.len()).collect()
    };
    // Incremental byte accounting (WI-160): the shed loop walks the running
    // total instead of re-summing the hot set each pass.
    let mut hot_bytes = hot
        .iter()
        .filter_map(|&idx| slots[idx].wire_len)
        .sum::<u64>();
    // Budget shed, oldest first. Retention priority (BUG-114): an explicit
    // reload_image pull this turn outranks unpinned recency — shed the
    // oldest UNpinned slot first, and only touch pins once the hot set is
    // pins-only. The newest slot is never a victim: a single oversized
    // image still rides (an empty-vision request would blind the model to
    // what the user just sent).
    let newest = slots.len() - 1;
    while !disabled && (hot.len() > cfg.max_images || hot_bytes > cfg.max_request_bytes) {
        let victim = hot
            .iter()
            .copied()
            .filter(|&idx| idx != newest && !pinned(&slots[idx]))
            .min()
            .or_else(|| hot.iter().copied().filter(|&idx| idx != newest).min());
        match victim {
            Some(victim) => {
                hot.remove(&victim);
                if let Some(wire) = slots[victim].wire_len {
                    hot_bytes = hot_bytes.saturating_sub(wire);
                }
            }
            // Only the newest slot remains: keep it as the floor.
            None => break,
        }
    }

    for (idx, slot) in slots.iter().enumerate() {
        // Inline legacy images carry their data URL already — a hot one is
        // left exactly as logged.
        if hot.contains(&idx) {
            if let Some(url) = slot
                .file_id
                .as_deref()
                .and_then(|file_id| resolver.resolve(file_id))
            {
                replace_slot(
                    messages,
                    slot,
                    ContentBlock::Image {
                        attachment: JsonValue::Object(vec![(
                            "url".into(),
                            JsonValue::String(url),
                        )]),
                    },
                );
                continue;
            }
            if slot.file_id.is_none() {
                continue;
            }
            // Referenced but unresolvable (asset pruned): degrade to text
            // rather than shipping a dead reference.
        }
        // A pinned slot that still went cold was displaced by the budget:
        // the pull already happened this turn, so its placeholder must NOT
        // re-issue the reload instruction (repeating the call cannot help).
        let displaced = pinned(slot) && !hot.contains(&idx);
        replace_slot(
            messages,
            slot,
            ContentBlock::Text(cold_text(slot.file_id.as_deref(), hint, displaced)),
        );
    }
}

/// Metadata placeholder replacing one cold image: identity + recovery path,
/// no content summary (WI-152 S3 — text-only/offload placeholder parity).
/// `displaced` marks an image whose reload pull this turn was pinned out by
/// the request budget — the text states the pull already happened instead of
/// inviting another call.
fn cold_text(file_id: Option<&str>, hint: ColdHint, displaced: bool) -> String {
    let short = || {
        file_id
            .map(|id| {
                let s = id.strip_prefix("sha256:").unwrap_or(id);
                &s[..s.len().min(8)]
            })
            .unwrap_or("")
    };
    match file_id {
        Some(file_id) => match hint {
            ColdHint::Reload => {
                if displaced {
                    format!(
                        "[历史图片未随请求发送 file_id={file_id}(本回合已重新加载过,因热图预算\
                         (张数/字节上限)被挤回冷态;重复调用 {RELOAD_IMAGE_TOOL} 无效,请基于当前\
                         可见图片继续)]"
                    )
                } else {
                    format!(
                        "[历史图片未随请求发送 file_id={file_id}(如需查看原图,调用 {RELOAD_IMAGE_TOOL} \
                         并传该 file_id)]"
                    )
                }
            }
            ColdHint::TextOnly => {
                format!("[图片已省略：本模型仅支持文本输入；附件 {}]", short())
            }
            ColdHint::BudgetDisabled => {
                format!(
                    "[图片已省略：热图预算(张数/字节)配置为 0,本请求不携带图片;附件 {}]",
                    short()
                )
            }
        },
        None => "[历史图片(内联数据,未引用化;不可按需取回)]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::llm::{MessageId, MessageRole, MessageSource};
    use std::collections::HashMap;

    struct MockResolver {
        urls: HashMap<String, String>,
        lengths: HashMap<String, u64>,
    }

    impl MockResolver {
        /// Resolves every listed id with a 3000-byte payload (wire length
        /// 4000 — budget assertions below use multiples of 4000).
        fn covering(ids: &[&str]) -> Self {
            Self::sized(
                &ids.iter()
                    .map(|id| (*id, 3000u64))
                    .collect::<Vec<_>>(),
            )
        }

        /// Resolves ids with explicit payload byte lengths.
        fn sized(ids: &[(&str, u64)]) -> Self {
            Self {
                urls: ids
                    .iter()
                    .map(|(id, _)| (id.to_string(), format!("data:image/png;base64,{id}")))
                    .collect(),
                lengths: ids
                    .iter()
                    .map(|(id, bytes)| (id.to_string(), *bytes))
                    .collect(),
            }
        }
    }

    impl ImageResolver for MockResolver {
        fn resolve(&self, file_id: &str) -> Option<String> {
            self.urls.get(file_id).cloned()
        }
        fn byte_len(&self, file_id: &str) -> Option<u64> {
            self.lengths.get(file_id).copied()
        }
    }

    fn cfg(max_images: usize, max_request_bytes: u64) -> ImageProjectionConfig {
        ImageProjectionConfig {
            max_images,
            max_request_bytes,
            allow_reload: true,
        }
    }

    fn cfg_no_reload(max_images: usize, max_request_bytes: u64) -> ImageProjectionConfig {
        ImageProjectionConfig {
            max_images,
            max_request_bytes,
            allow_reload: false,
        }
    }

    fn image_message(file_id: &str) -> Message {
        Message {
            id: MessageId(format!("m-{file_id}")),
            role: MessageRole::User,
            content: vec![ContentBlock::Image {
                attachment: JsonValue::Object(vec![(
                    "fileId".into(),
                    JsonValue::String(file_id.to_string()),
                )]),
            }],
            source: MessageSource::User,
        }
    }

    fn text_message(text: &str) -> Message {
        Message {
            id: MessageId(text.to_string()),
            role: MessageRole::User,
            content: vec![ContentBlock::Text(text.to_string())],
            source: MessageSource::User,
        }
    }

    fn block_kind(block: &ContentBlock) -> &'static str {
        match block {
            ContentBlock::Image { .. } => "image",
            ContentBlock::Text(_) => "text",
            _ => "other",
        }
    }

    fn kinds(messages: &[Message]) -> Vec<&'static str> {
        messages
            .iter()
            .flat_map(|m| m.content.iter().map(block_kind))
            .collect()
    }

    /// WI-159: small images pack — within the byte budget everything rides.
    #[test]
    fn images_within_the_byte_budget_all_ride() {
        let mut messages = vec![
            image_message("sha256:a"),
            image_message("sha256:b"),
            image_message("sha256:c"),
            text_message("in between"),
            image_message("sha256:d"),
        ];
        // Each wire length is 4000; budget 16000 fits all four.
        let resolver = MockResolver::covering(&["sha256:a", "sha256:b", "sha256:c", "sha256:d"]);
        project_images(&mut messages, &cfg(8, 16_000), &resolver, &HashSet::new());
        assert_eq!(
            kinds(&messages),
            vec!["image", "image", "image", "text", "image"],
            "budget-fitting history stays fully hot — no fixed window"
        );
    }

    /// WI-159: over the byte budget the OLDEST slots shed; the newest stays.
    #[test]
    fn byte_budget_sheds_oldest_first() {
        let mut messages = vec![
            image_message("sha256:a"),
            image_message("sha256:b"),
            image_message("sha256:c"),
            image_message("sha256:d"),
        ];
        // 4 × 4000 wire > 12000 budget → shed the oldest (a).
        let resolver = MockResolver::covering(&["sha256:a", "sha256:b", "sha256:c", "sha256:d"]);
        project_images(&mut messages, &cfg(8, 12_000), &resolver, &HashSet::new());
        assert_eq!(kinds(&messages), vec!["text", "image", "image", "image"]);
        let ContentBlock::Text(cold) = &messages[0].content[0] else {
            panic!("oldest sheds to placeholder");
        };
        assert!(cold.contains("file_id=sha256:a"), "{cold}");
        assert!(cold.contains(RELOAD_IMAGE_TOOL), "{cold}");
    }

    /// WI-159: large images take more of the budget — a big image forces the
    /// small old ones out under a budget that would otherwise fit them.
    #[test]
    fn large_images_take_more_of_the_budget() {
        let mut messages = vec![
            image_message("sha256:a"),
            image_message("sha256:b"),
            image_message("sha256:big"),
        ];
        // wire: a=4000, b=4000, big=12000 → budget 12000 sheds a and b;
        // the big newest still rides (newest-slot floor).
        let resolver = MockResolver::sized(&[
            ("sha256:a", 3000),
            ("sha256:b", 3000),
            ("sha256:big", 9000),
        ]);
        project_images(&mut messages, &cfg(8, 12_000), &resolver, &HashSet::new());
        assert_eq!(kinds(&messages), vec!["text", "text", "image"]);
    }

    /// BUG-69 regression (WI-159 re-statement): the count cap holds even
    /// with a generous byte budget — at most `max_images` ride.
    #[test]
    fn count_cap_sheds_oldest_beyond_max_images() {
        let mut messages = vec![
            image_message("sha256:a"),
            image_message("sha256:b"),
            image_message("sha256:c"),
            image_message("sha256:d"),
            image_message("sha256:e"),
        ];
        let resolver =
            MockResolver::covering(&["sha256:a", "sha256:b", "sha256:c", "sha256:d", "sha256:e"]);
        project_images(&mut messages, &cfg(2, u64::MAX), &resolver, &HashSet::new());
        assert_eq!(kinds(&messages), vec!["text", "text", "text", "image", "image"]);
    }

    /// A single image larger than the whole budget still rides — the newest
    /// slot is the floor (an empty-vision request would blind the model to
    /// what the user just sent).
    #[test]
    fn single_oversize_image_still_rides_as_the_newest() {
        let mut messages = vec![image_message("sha256:huge")];
        let resolver = MockResolver::sized(&[("sha256:huge", 9000)]);
        project_images(&mut messages, &cfg(8, 1_000), &resolver, &HashSet::new());
        assert_eq!(kinds(&messages), vec!["image"]);
    }

    /// BUG-114 semantics (storm regression): reload pulls outrank unpinned
    /// recency — the pins keep their seats and the OLDEST unpinned slot
    /// yields first, whatever the shed trigger (count cap here).
    #[test]
    fn reload_pulls_beat_recency_but_budget_holds() {
        let mut messages = vec![
            image_message("sha256:a"),
            image_message("sha256:b"),
            image_message("sha256:c"),
            image_message("sha256:d"),
            image_message("sha256:e"),
        ];
        let reloads = HashSet::from(["sha256:a".to_string(), "sha256:b".to_string()]);
        // 5 images > max_images 3 → shed the oldest UNpinned members c, d;
        // both pins a and b stay visible alongside the newest.
        let resolver =
            MockResolver::covering(&["sha256:a", "sha256:b", "sha256:c", "sha256:d", "sha256:e"]);
        project_images(&mut messages, &cfg(3, u64::MAX), &resolver, &reloads);
        assert_eq!(kinds(&messages), vec!["image", "image", "text", "text", "image"]);
    }

    /// BUG-114 regression (the observed livelock shape): two historical
    /// images reloaded in one turn must BOTH stay visible alongside the
    /// newest image — the second pull must not silently revoke the first
    /// one's seat.
    #[test]
    fn second_reload_keeps_the_first_pull_hot() {
        let mut messages = vec![
            image_message("sha256:a"),
            image_message("sha256:b"),
            image_message("sha256:c"),
            image_message("sha256:d"),
            image_message("sha256:w1"),
            image_message("sha256:w2"),
        ];
        let reloads = HashSet::from(["sha256:a".to_string(), "sha256:b".to_string()]);
        // 6 images > max_images 3 → the oldest unpinned members (c, d, w1)
        // shed; a, b and the newest w2 ride the wire.
        let resolver = MockResolver::covering(&[
            "sha256:a",
            "sha256:b",
            "sha256:c",
            "sha256:d",
            "sha256:w1",
            "sha256:w2",
        ]);
        project_images(&mut messages, &cfg(3, u64::MAX), &resolver, &reloads);
        assert_eq!(
            kinds(&messages),
            vec!["image", "image", "text", "text", "text", "image"],
            "both reload pulls must stay hot; unpinned slots shed first"
        );
    }

    /// BUG-114: once the shed reaches the pins, the OLDEST pin is
    /// displaced — and a displaced pin's placeholder states the pull
    /// already happened instead of re-issuing the reload instruction. The
    /// newest slot (w2) outranks pins: it never sheds, so the second victim
    /// is already the oldest pin.
    #[test]
    fn displaced_pin_placeholder_has_no_retry_instruction() {
        let mut messages = vec![
            image_message("sha256:a"),
            image_message("sha256:b"),
            image_message("sha256:c"),
            image_message("sha256:d"),
            image_message("sha256:w1"),
            image_message("sha256:w2"),
        ];
        // 6 > max_images 3: unpinned w1 sheds first, but w2 is the newest
        // (floor) → the next two victims are the oldest pins a, b;
        // hot = {c, d, w2}.
        let reloads = HashSet::from([
            "sha256:a".to_string(),
            "sha256:b".to_string(),
            "sha256:c".to_string(),
            "sha256:d".to_string(),
        ]);
        let resolver = MockResolver::covering(&[
            "sha256:a",
            "sha256:b",
            "sha256:c",
            "sha256:d",
            "sha256:w1",
            "sha256:w2",
        ]);
        project_images(&mut messages, &cfg(3, u64::MAX), &resolver, &reloads);
        assert_eq!(
            kinds(&messages),
            vec!["text", "text", "image", "image", "text", "image"]
        );
        for displaced_at in [0usize, 1] {
            let ContentBlock::Text(displaced) = &messages[displaced_at].content[0] else {
                panic!("the displaced pin at slot {displaced_at} becomes text");
            };
            assert!(displaced.contains("已重新加载过"), "{displaced}");
            assert!(displaced.contains("无效"), "{displaced}");
            assert!(
                !displaced.contains("如需查看原图"),
                "a displaced pin must not re-issue the reload instruction: {displaced}"
            );
        }
        // The never-reloaded images keep the normal recovery hint.
        let ContentBlock::Text(cold) = &messages[4].content[0] else {
            panic!("shed unpinned member becomes text");
        };
        assert!(cold.contains("如需查看原图"), "{cold}");
        assert!(cold.contains(RELOAD_IMAGE_TOOL), "{cold}");
    }

    /// WI-152 S2: text-only runs (`allow_reload = false`, (0, 0) budget)
    /// get the explicit text-only wording, no reload hint — and a reload
    /// pull must not force the image hot.
    #[test]
    fn text_only_projection_sheds_everything() {
        let mut messages = vec![image_message("sha256:old"), image_message("sha256:new")];
        let resolver = MockResolver::covering(&["sha256:old", "sha256:new"]);
        let mut reloads = HashSet::new();
        reloads.insert("sha256:old".to_string());
        project_images(
            &mut messages,
            &cfg_no_reload(0, 0),
            &resolver,
            &reloads,
        );
        let ContentBlock::Text(old) = &messages[0].content[0] else {
            panic!("cold image becomes text");
        };
        assert!(old.contains("本模型仅支持文本输入"), "{old}");
        assert!(
            old.contains("附件 old"),
            "short file id rides the placeholder: {old}"
        );
        assert!(!old.contains(RELOAD_IMAGE_TOOL), "{old}");
        assert!(
            matches!(messages[1].content[0], ContentBlock::Text(_)),
            "text-only runs never put image data on the wire"
        );
    }

    #[test]
    fn unresolvable_hot_id_falls_back_to_cold_text() {
        // Budget-known (byte_len) but unresolvable (no data URL): the hot
        // slot degrades to text instead of shipping a dead reference.
        struct BytesOnly;
        impl ImageResolver for BytesOnly {
            fn resolve(&self, _file_id: &str) -> Option<String> {
                None
            }
            fn byte_len(&self, _file_id: &str) -> Option<u64> {
                Some(3000)
            }
        }
        let mut messages = vec![image_message("sha256:gone")];
        project_images(
            &mut messages,
            &cfg(2, u64::MAX),
            &BytesOnly,
            &HashSet::new(),
        );
        assert!(matches!(messages[0].content[0], ContentBlock::Text(_)));
    }

    /// Legacy inline slots budget on their base64 payload — the
    /// `data:...;base64,` framing is not image bytes (BUG-124: the prefix
    /// used to inflate every inline slot's cost).
    #[test]
    fn legacy_inline_images_budget_on_their_payload() {
        let inline = || ContentBlock::Image {
            attachment: JsonValue::Object(vec![(
                "url".into(),
                JsonValue::String("data:image/png;base64,AAAA".into()),
            )]),
        };
        let make = || {
            vec![
                Message {
                    id: MessageId("m1".into()),
                    role: MessageRole::User,
                    content: vec![inline()],
                    source: MessageSource::User,
                },
                Message {
                    id: MessageId("m2".into()),
                    role: MessageRole::User,
                    content: vec![inline()],
                    source: MessageSource::User,
                },
            ]
        };
        // Each payload is 4 chars ("AAAA"); budget 4 keeps only the newest.
        let mut messages = make();
        project_images(
            &mut messages,
            &cfg(8, 4),
            &MockResolver::sized(&[]),
            &HashSet::new(),
        );
        assert!(matches!(messages[0].content[0], ContentBlock::Text(ref t) if t.contains("内联")));
        assert!(matches!(messages[1].content[0], ContentBlock::Image { .. }));
        // Budget 8 fits both — the 26-char URL framing does NOT count.
        let mut messages = make();
        project_images(
            &mut messages,
            &cfg(8, 8),
            &MockResolver::sized(&[]),
            &HashSet::new(),
        );
        assert!(matches!(messages[0].content[0], ContentBlock::Image { .. }));
        assert!(matches!(messages[1].content[0], ContentBlock::Image { .. }));
    }

    /// Budget-agnostic resolvers (`byte_len` → None) fall back to the count
    /// cap alone: unknown sizes never block images from riding.
    #[test]
    fn unknown_lengths_fall_back_to_the_count_cap() {
        struct UrlOnly;
        impl ImageResolver for UrlOnly {
            fn resolve(&self, file_id: &str) -> Option<String> {
                Some(format!("data:image/png;base64,{file_id}"))
            }
        }
        let mut messages = vec![
            image_message("sha256:a"),
            image_message("sha256:b"),
            image_message("sha256:c"),
        ];
        project_images(
            &mut messages,
            &cfg(2, 1), // byte budget absurdly low but unknown sizes don't count
            &UrlOnly,
            &HashSet::new(),
        );
        assert_eq!(kinds(&messages), vec!["text", "image", "image"]);
    }

    #[test]
    fn image_free_surface_is_untouched() {
        let mut messages = vec![text_message("no images here")];
        let before = format!("{:?}", messages);
        project_images(
            &mut messages,
            &cfg(2, 4_000),
            &MockResolver::covering(&["sha256:x"]),
            &HashSet::new(),
        );
        assert_eq!(format!("{:?}", messages), before);
    }

    /// BUG-119: images nested inside a ToolResult join the budget — an
    /// oversized tool-result image sheds to a placeholder INSIDE the tool
    /// result, and a reload pin reaches nested slots too.
    #[test]
    fn nested_tool_result_images_join_the_budget() {
        use starriver_kernel::llm::CallId;
        let nested_image = |file_id: &str| {
            ContentBlock::Image {
                attachment: JsonValue::Object(vec![(
                    "fileId".into(),
                    JsonValue::String(file_id.to_string()),
                )]),
            }
        };
        let tool_result = |inner: Vec<ContentBlock>| ContentBlock::ToolResult {
            call_id: CallId("c1".into()),
            content: inner,
            is_error: false,
        };
        // Message 0: a tool result embedding two images; message 1: one
        // top-level image. Three slots total, wire 4000 each.
        let mut messages = vec![
            Message {
                id: MessageId("m-tool".into()),
                role: MessageRole::User,
                content: vec![tool_result(vec![
                    nested_image("sha256:n1"),
                    nested_image("sha256:n2"),
                ])],
                source: MessageSource::User,
            },
            image_message("sha256:top"),
        ];
        // Budget 8000 sheds the oldest slot (n1, nested).
        let resolver = MockResolver::covering(&["sha256:n1", "sha256:n2", "sha256:top"]);
        project_images(&mut messages, &cfg(8, 8_000), &resolver, &HashSet::new());
        let ContentBlock::ToolResult { content, .. } = &messages[0].content[0] else {
            panic!("tool result survives");
        };
        assert!(
            matches!(&content[0], ContentBlock::Text(t) if t.contains("file_id=sha256:n1")),
            "the shed nested image becomes a placeholder INSIDE the tool result"
        );
        assert!(matches!(&content[1], ContentBlock::Image { .. }));
        assert!(matches!(&messages[1].content[0], ContentBlock::Image { .. }));

        // A reload pin reaches the nested slot: n1 comes back hot, and the
        // next-oldest unpinned (n2) sheds instead.
        let mut messages = vec![
            Message {
                id: MessageId("m-tool".into()),
                role: MessageRole::User,
                content: vec![tool_result(vec![
                    nested_image("sha256:n1"),
                    nested_image("sha256:n2"),
                ])],
                source: MessageSource::User,
            },
            image_message("sha256:top"),
        ];
        let reloads = HashSet::from(["sha256:n1".to_string()]);
        project_images(&mut messages, &cfg(8, 8_000), &resolver, &reloads);
        let ContentBlock::ToolResult { content, .. } = &messages[0].content[0] else {
            panic!("tool result survives");
        };
        assert!(
            matches!(&content[0], ContentBlock::Image { .. }),
            "the pinned nested image rides"
        );
        assert!(
            matches!(&content[1], ContentBlock::Text(t) if t.contains("file_id=sha256:n2")),
            "the unpinned nested sibling sheds instead"
        );
    }

    /// BUG-118: a configured projection with a missing resolver degrades
    /// every fileId slot to its cold placeholder instead of shipping the
    /// raw reference block; inline slots still ride.
    #[test]
    fn missing_resolver_degrades_file_id_slots_to_placeholders() {
        let mut messages = vec![
            image_message("sha256:a"),
            Message {
                id: MessageId("m-inline".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Image {
                    attachment: JsonValue::Object(vec![(
                        "url".into(),
                        JsonValue::String("data:image/png;base64,AAAA".into()),
                    )]),
                }],
                source: MessageSource::User,
            },
        ];
        project_images(
            &mut messages,
            &cfg(8, u64::MAX),
            &MissingResolver,
            &HashSet::new(),
        );
        let ContentBlock::Text(cold) = &messages[0].content[0] else {
            panic!("fileId slot must not ride raw without a resolver");
        };
        assert!(cold.contains("file_id=sha256:a"), "{cold}");
        assert!(
            matches!(&messages[1].content[0], ContentBlock::Image { .. }),
            "inline data URLs need no resolver"
        );
    }

    #[test]
    fn reload_argument_parser_extracts_file_id() {
        let args = |text: &str| JsonValue::from_json_str(text).unwrap();
        assert_eq!(
            parse_reload_file_id(&args(r#"{"file_id": "sha256:abc"}"#)),
            Some("sha256:abc".to_string())
        );
        assert_eq!(parse_reload_file_id(&args(r#"{"other": 1}"#)), None);
        // A non-object argument (the ingress keeps raw text as a String)
        // carries no file id.
        assert_eq!(parse_reload_file_id(&JsonValue::Null), None);
    }

    /// WI-160: a zero in EITHER budget dimension disables images — the
    /// newest-slot floor must NOT keep an image riding, and the placeholder
    /// must not advertise a reload that could never take effect.
    #[test]
    fn single_zero_budget_dimension_disables_images_entirely() {
        let mut messages = vec![image_message("sha256:a"), image_message("sha256:new")];
        let resolver = MockResolver::covering(&["sha256:a", "sha256:new"]);
        // Count zero, bytes generous.
        project_images(
            &mut messages,
            &cfg(0, u64::MAX),
            &resolver,
            &HashSet::new(),
        );
        assert!(matches!(messages[1].content[0], ContentBlock::Text(_)));
        let ContentBlock::Text(disabled) = &messages[1].content[0] else {
            unreachable!()
        };
        assert!(disabled.contains("热图预算"), "{disabled}");
        assert!(!disabled.contains(RELOAD_IMAGE_TOOL), "{disabled}");
        assert!(!disabled.contains("仅支持文本输入"), "{disabled}");

        // Bytes zero, count generous — same outcome.
        let mut messages = vec![image_message("sha256:a"), image_message("sha256:new")];
        project_images(
            &mut messages,
            &cfg(8, 0),
            &resolver,
            &HashSet::new(),
        );
        assert!(matches!(messages[1].content[0], ContentBlock::Text(_)));

        // And a reload pull must not force anything hot under a disabled
        // budget.
        let mut messages = vec![image_message("sha256:a"), image_message("sha256:new")];
        let mut reloads = HashSet::new();
        reloads.insert("sha256:new".to_string());
        project_images(&mut messages, &cfg(0, u64::MAX), &resolver, &reloads);
        assert!(
            matches!(messages[1].content[0], ContentBlock::Text(_)),
            "pins cannot ride a disabled budget"
        );
    }

    /// WI-160: the same file id pasted twice occupies two slots — byte
    /// accounting counts each occurrence, and one reload pins both.
    #[test]
    fn duplicate_slots_share_the_pin_and_double_the_budget() {
        let mut messages = vec![
            image_message("sha256:same"),
            image_message("sha256:other"),
            image_message("sha256:same"),
        ];
        // wire: same=4000, other=4000, same=4000 → budget 8000 sheds the
        // oldest unpinned (slot 0).
        let resolver = MockResolver::covering(&["sha256:same", "sha256:other"]);
        project_images(&mut messages, &cfg(8, 8_000), &resolver, &HashSet::new());
        assert_eq!(kinds(&messages), vec!["text", "image", "image"]);

        // Reloading "same" pins BOTH of its slots; "other" sheds instead.
        let mut messages = vec![
            image_message("sha256:same"),
            image_message("sha256:other"),
            image_message("sha256:same"),
        ];
        let reloads = HashSet::from(["sha256:same".to_string()]);
        project_images(&mut messages, &cfg(8, 8_000), &resolver, &reloads);
        assert_eq!(kinds(&messages), vec!["image", "text", "image"]);
    }

    /// WI-160: floor + pin on the same slot — an oversized NEWEST image that
    /// was also reloaded rides (floor wins, no self-displacement).
    #[test]
    fn newest_pinned_oversize_slot_rides() {
        let mut messages = vec![image_message("sha256:small"), image_message("sha256:huge")];
        let resolver = MockResolver::sized(&[("sha256:small", 3000), ("sha256:huge", 9000)]);
        let reloads = HashSet::from(["sha256:huge".to_string()]);
        // Budget fits `small` alone; the huge newest is both newest (floor)
        // and pinned — it rides, small sheds.
        project_images(&mut messages, &cfg(8, 4_000), &resolver, &reloads);
        assert_eq!(kinds(&messages), vec!["text", "image"]);
    }

    /// WI-160: max_images=1 — only the newest survives; an older pin is
    /// displaced with the no-retry placeholder (never the newest floor).
    #[test]
    fn max_images_one_keeps_only_the_newest() {
        let mut messages = vec![
            image_message("sha256:a"),
            image_message("sha256:b"),
            image_message("sha256:c"),
        ];
        let resolver = MockResolver::covering(&["sha256:a", "sha256:b", "sha256:c"]);
        let reloads = HashSet::from(["sha256:a".to_string(), "sha256:b".to_string()]);
        project_images(&mut messages, &cfg(1, u64::MAX), &resolver, &reloads);
        assert_eq!(kinds(&messages), vec!["text", "text", "image"]);
        for slot in [0usize, 1] {
            let ContentBlock::Text(text) = &messages[slot].content[0] else {
                panic!("slot {slot} sheds");
            };
            assert!(text.contains("已重新加载过"), "{}", text);
            assert!(!text.contains("如需查看原图"), "{}", text);
        }
    }

    /// WI-160: budget exactly at the boundary is inclusive — shedding stops
    /// the moment both dimensions fit (no off-by-one).
    #[test]
    fn budget_boundary_is_inclusive() {
        let resolver = MockResolver::covering(&["sha256:a", "sha256:b", "sha256:c"]);
        // 3 × 4000 wire == 12000 budget: all ride.
        let mut messages = vec![
            image_message("sha256:a"),
            image_message("sha256:b"),
            image_message("sha256:c"),
        ];
        project_images(&mut messages, &cfg(3, 12_000), &resolver, &HashSet::new());
        assert_eq!(kinds(&messages), vec!["image", "image", "image"]);
        // One byte less sheds exactly one (fresh surface — projection
        // rewrites hot slots to url form, so each run starts from refs).
        let mut messages = vec![
            image_message("sha256:a"),
            image_message("sha256:b"),
            image_message("sha256:c"),
        ];
        project_images(&mut messages, &cfg(3, 11_999), &resolver, &HashSet::new());
        assert_eq!(kinds(&messages), vec!["text", "image", "image"]);
    }

    /// WI-160: mixed known/unknown lengths — unknown slots ride free of the
    /// byte budget but still count against the image count.
    #[test]
    fn unknown_lengths_mix_with_known_under_the_count_cap() {
        struct Mixed {
            known: MockResolver,
        }
        impl ImageResolver for Mixed {
            fn resolve(&self, file_id: &str) -> Option<String> {
                self.known.resolve(file_id)
            }
            fn byte_len(&self, file_id: &str) -> Option<u64> {
                // Only "sha256:a" reports a length.
                (file_id == "sha256:a").then(|| 3000)
            }
        }
        let mut messages = vec![
            image_message("sha256:a"),
            image_message("sha256:unk1"),
            image_message("sha256:unk2"),
        ];
        // Byte budget fits nothing known (4000 > 1), but unknowns are free:
        // the count cap 2 sheds the oldest unpinned (a)… wait — a is KNOWN
        // and over budget alone, so the byte loop sheds it even though the
        // count (3) also exceeds; unknowns survive on the count cap alone.
        project_images(
            &mut messages,
            &cfg(2, 1),
            &Mixed {
                known: MockResolver::covering(&["sha256:a", "sha256:unk1", "sha256:unk2"]),
            },
            &HashSet::new(),
        );
        assert_eq!(kinds(&messages), vec!["text", "image", "image"]);
    }
}
