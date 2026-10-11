//! Time-context snapshot (SP4, prompt-parity §2.3): sampling of the
//! local clock injected as a pre-step history snapshot — first sample
//! mandatory, then throttled by `refresh_interval_ms` (`Some(0)` = every step,
//! diff-suppressed). `None` on the agent options disables the projector.

use starriver_kernel::llm::{mint_message_id, ContentBlock, Message, MessageRole, MessageSource};
use starriver_kernel::session::Session;

/// Message-source tag used by the projection.
pub const TIME_PLUGIN: &str = "time-context";

/// Change-only + throttled sampler. `now_ms` is injected so tests can drive
/// the clock deterministically.
pub struct TimeProjection {
    /// Digest of the last sent text (restored across restarts).
    last_digest: Option<u64>,
    /// Wall-clock anchor of the last send: the live send time, or — after a
    /// restart — the persisted event timestamp of the last owned snapshot, so
    /// the window survives agent recreation and process restarts alike.
    anchor_ms: Option<u64>,
}

impl TimeProjection {
    pub fn new(session: &Session) -> Self {
        let restored = restore(session);
        Self {
            last_digest: restored.map(|(d, _)| d),
            anchor_ms: restored.map(|(_, t)| t),
        }
    }

    pub fn project(
        &mut self,
        now_ms: u64,
        turn: u64,
        step: u64,
        interval_ms: Option<u64>,
        preceding_message_ms: Option<u64>,
    ) -> Option<Message> {
        let text = render(now_ms, turn, step, preceding_message_ms);
        let next = crate::instructions::digest(&text);
        // Throttle first — the text always differs (turn/step/iso), so the
        // window is the primary gate. An unchanged text is diff-suppressed.
        if self.anchor_ms.is_some()
            && interval_ms
                .is_some_and(|window| now_ms.saturating_sub(self.anchor_ms.unwrap()) < window)
        {
            return None;
        }
        if self.last_digest == Some(next) {
            return None;
        }
        self.anchor_ms = Some(now_ms);
        self.last_digest = Some(next);
        Some(Message {
            id: mint_message_id(),
            role: MessageRole::User,
            content: vec![ContentBlock::Text(text)],
            source: MessageSource::Plugin {
                plugin: TIME_PLUGIN.into(),
                form: None,
            },
        })
    }
}

fn render(now_ms: u64, turn: u64, step: u64, preceding_message_ms: Option<u64>) -> String {
    let iso = chrono::DateTime::from_timestamp_millis(now_ms as i64)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y-%m-%dT%H:%M:%S%:z")
                .to_string()
        })
        .unwrap_or_else(|| format!("{now_ms}"));
 // vocabulary: the baseline is the model-visible surface, so the
    // elapsed measure runs against the preceding message, not the last
    // sample; the first sample has none and reports `unavailable`.
    let baseline = if step == 1 {
        "model-visible message"
    } else {
        "step context"
    };
    let elapsed = preceding_message_ms
        .map(|ms| format_duration(now_ms.saturating_sub(ms)))
        .unwrap_or_else(|| "unavailable".into());
    format!(
        "Time sampled while preparing turn {turn}, step {step}: {iso}\nElapsed since the preceding {baseline}: {elapsed}."
    )
}

/// Compact whole-second units (formatDuration parity): zero components
/// omitted, seconds always present — e.g. `90s`, `5m 30s`, `2h 0s`.
fn format_duration(elapsed_ms: u64) -> String {
    let mut seconds = elapsed_ms / 1000;
    let days = seconds / 86_400;
    seconds %= 86_400;
    let hours = seconds / 3600;
    seconds %= 3600;
    let minutes = seconds / 60;
    seconds %= 60;
    let mut parts: Vec<String> = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    parts.push(format!("{seconds}s"));
    parts.join(" ")
}

/// Find the last owned time snapshot: its text digest plus the persisted
/// event timestamp (the throttle anchor across agent recreation/restarts).
fn restore(session: &Session) -> Option<(u64, u64)> {
    for event in session.events().iter().rev() {
        if event.event_type != "user/message" {
            continue;
        }
        let Ok(message) = Message::from_json(event.data.clone()) else {
            continue;
        };
        let owned = matches!(&message.source,
            MessageSource::Plugin { plugin, .. } if plugin == TIME_PLUGIN);
        if !owned {
            continue;
        }
        let text = message
            .content
            .first()
            .and_then(|b| match b {
                ContentBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap_or_default();
        return Some((crate::instructions::digest(&text), event.time));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::session::{AppendOptions, SessionId, SurfaceOp};

    fn fresh() -> Session {
        Session::new(SessionId("t".into()), cordis_rslite::EventBus::new())
    }

    fn text_of(message: &Message) -> String {
        match message.content.first() {
            Some(ContentBlock::Text(t)) => t.clone(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn first_sample_is_mandatory_and_wording_is_stable() {
        let mut p = TimeProjection::new(&fresh());
        let message = p
            .project(1_000, 3, 1, Some(60_000), None)
            .expect("first sample");
        let text = text_of(&message);
        assert!(text.starts_with("Time sampled while preparing turn 3, step 1: "));
        assert!(
            text.ends_with("Elapsed since the preceding model-visible message: unavailable."),
            "step 1 baseline + first-sample unavailable: {text}"
        );
        // Local offset present in the ISO segment (+HH:MM or Z).
        let iso = text.split(": ").nth(1).unwrap_or("");
        assert!(
            iso.contains('+') || iso.contains('-') || iso.contains('Z'),
            "offset-aware local time expected: {text}"
        );
    }

    #[test]
    fn format_duration_matches_the_compact_shape() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(90_000), "1m 30s");
        assert_eq!(format_duration(330_000), "5m 30s");
        assert_eq!(format_duration(7_200_000), "2h 0s");
        assert_eq!(format_duration(93_600_000), "1d 2h 0s");
    }

    #[test]
    fn throttle_window_suppresses_then_releases_with_elapsed() {
        let mut p = TimeProjection::new(&fresh());
        let _ = p.project(1_000, 1, 1, Some(60_000), None).unwrap();
        // Inside the window: silent.
        assert!(p.project(30_000, 2, 1, Some(60_000), None).is_none());
        // Outside the window: sent against the step≥2 vocabulary, elapsed vs
        // the preceding model-visible message (60s before now).
        let second = p.project(61_500, 2, 2, Some(60_000), Some(1_500)).unwrap();
        assert!(
            text_of(&second).ends_with("Elapsed since the preceding step context: 1m 0s."),
            "{}",
            text_of(&second)
        );
    }

    #[test]
    fn zero_interval_attempts_every_step_but_diff_suppresses_identical_text() {
        let mut p = TimeProjection::new(&fresh());
        let _ = p.project(1_000, 1, 1, Some(0), None).unwrap();
        // Same now/turn/step → identical text → diff-suppressed.
        assert!(p.project(1_000, 1, 1, Some(0), None).is_none());
        // New step → new text → sent.
        let _ = p.project(1_000, 1, 2, Some(0), None).unwrap();
    }

    #[test]
    fn restart_inherits_the_persisted_throttle_anchor() {
        let session = fresh();
        let mut first = TimeProjection::new(&session);
        let message = first.project(1_000, 1, 1, Some(60_000), None).unwrap();
        session
            .append_with(
                "user/message",
                message.to_json().unwrap(),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: Vec::new(),
                },
            )
            .unwrap();
        let sent_at = session.events().last().unwrap().time;

        // A recreated agent (resume, or a fresh process) inherits the anchor:
        // inside the window it stays silent; past it, it samples again.
        let mut restarted = TimeProjection::new(&session);
        assert!(
            restarted
                .project(sent_at + 30_000, 1, 2, Some(60_000), None)
                .is_none(),
            "resume inside the window must not resample"
        );
        assert!(restarted
            .project(sent_at + 61_000, 1, 2, Some(60_000), None)
            .is_some());
    }

    #[test]
    fn none_interval_attempts_every_step_diff_suppressed() {
        let mut p = TimeProjection::new(&fresh());
        assert!(
            p.project(1_000, 1, 1, None, None).is_some(),
            "first mandatory"
        );
        assert!(
            p.project(1_000, 1, 1, None, None).is_none(),
            "identical text suppressed"
        );
        assert!(
            p.project(1_000, 1, 2, None, None).is_some(),
            "new step → new text → sent"
        );
    }
}
