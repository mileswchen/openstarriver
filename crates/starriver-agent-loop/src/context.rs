//! Durable runtime-context projection: track the last retained snapshot and
//! project a new one only when the rendered value changes.

use starriver_kernel::context::AssembledContext;
use starriver_kernel::llm::{Message, MessageRole, MessageSource};
use starriver_kernel::session::{Session, SessionEvent};

pub const CONTEXT_PLUGIN: &str = "runtime-context";
pub const CLEARED: &str =
    "Current runtime context: none. Earlier runtime-context snapshots no longer apply.";

struct RetainedSnapshot {
    #[allow(dead_code)] // read by tests via restore.
    seq: u64,
    text: String,
}

pub struct RuntimeContextProjection {
    /// None = never had a snapshot; Some(None) = cleared; Some(Some(s)) = retained.
    retained: Option<Option<RetainedSnapshot>>,
}

impl RuntimeContextProjection {
    pub fn new(_ctx: &cordis_rslite::Context, session: &Session) -> Self {
        let retained = restore(&session.events(), &session.surface().nodes);
        Self { retained }
    }

    pub fn project(&mut self, current: &str, sections: Vec<AssembledContext>) -> Option<Message> {
        if self.retained.is_none() && current.is_empty() {
            return None;
        }
        let snapshot = if current.is_empty() { CLEARED } else { current };
        if self
            .retained
            .as_ref()
            .map(|r| r.as_ref().map(|s| s.text.as_str()))
            == Some(Some(snapshot))
        {
            return None;
        }
        // BUG-68: record what we are about to emit — a `&self` projection
        // never updated the retained state and re-emitted the same snapshot
        // on every step of the run.
        self.retained = Some(Some(RetainedSnapshot {
            seq: 0,
            text: snapshot.to_string(),
        }));
        Some(Message {
            id: starriver_kernel::llm::mint_message_id(),
            role: MessageRole::User,
            content: vec![starriver_kernel::llm::ContentBlock::Text(snapshot.into())],
            source: MessageSource::Plugin {
                plugin: CONTEXT_PLUGIN.into(),
                form: Some(starriver_kernel::llm::ContextForm::Snapshot {
                    truncated: false,
                    budget_bytes: 0,
                    composed_bytes: 0,
                    sections: sections
                        .into_iter()
                        .map(|c| starriver_kernel::llm::ContextSnapshotSection {
                            name: c.name,
                            text: c.text,
                        })
                        .collect(),
                }),
            },
        })
    }
}

fn restore(events: &[SessionEvent], surface_nodes: &[u64]) -> Option<Option<RetainedSnapshot>> {
    let mut found = None;
    for event in events.iter().rev() {
        if event.event_type != "user/message" {
            continue;
        }
        let Ok(message) = Message::from_json(event.data.clone()) else {
            continue;
        };
        let owned = matches!(
            &message.source,
            MessageSource::Plugin { plugin, .. } if plugin == CONTEXT_PLUGIN
        );
        if !owned {
            continue;
        }
        found = Some(event.seq);
        if surface_nodes.contains(&event.seq) {
            let text = message
                .content
                .first()
                .and_then(|b| match b {
                    starriver_kernel::llm::ContentBlock::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            return Some(Some(RetainedSnapshot {
                seq: event.seq,
                text,
            }));
        }
    }
    if found.is_some() {
        Some(None)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::EventBus;
    use starriver_kernel::session::SurfaceOp;

    fn session() -> Session {
        Session::new(
            starriver_kernel::session::SessionId("s1".into()),
            EventBus::new(),
        )
    }

    fn snapshot(seq: u64, text: &str) -> SessionEvent {
        let message = Message {
            id: starriver_kernel::llm::MessageId(format!("m{seq}")),
            role: MessageRole::User,
            content: vec![starriver_kernel::llm::ContentBlock::Text(text.into())],
            source: MessageSource::Plugin {
                plugin: CONTEXT_PLUGIN.into(),
                form: None,
            },
        };
        SessionEvent {
            seq,
            time: 1,
            event_type: "user/message".into(),
            data: message.to_json().unwrap(),
            surface_op: Some(SurfaceOp::Append),
            source_event_seqs: Vec::new(),
        }
    }

    /// BUG-68 regression (round 5): the module contract is "project a new
    /// one only when the rendered value changes" — a second project with an
    /// UNCHANGED value must stay silent, not re-emit the same snapshot every
    /// step (the surface then accumulates N duplicate snapshots per run).
    #[test]
    fn unchanged_value_projects_once() {
        let mut p = RuntimeContextProjection::new(&cordis_rslite::Context::new(), &session());
        let first = p.project("ctx-A", vec![]);
        assert!(first.is_some(), "the first snapshot fires");
        let second = p.project("ctx-A", vec![]);
        assert!(
            second.is_none(),
            "an unchanged value must not re-project (got {second:?})"
        );
    }

    #[test]
    fn restore_finds_last_surface_snapshot() {
        let events = vec![
            snapshot(0, "old"),
            snapshot(1, "old"), // same text, later
        ];
        let restored = restore(&events, &[1]).unwrap().unwrap();
        assert_eq!(restored.text, "old");
        assert_eq!(restored.seq, 1);
    }

    #[test]
    fn restore_cleared_when_snapshot_not_on_surface() {
        let events = vec![snapshot(0, "old")];
        let restored = restore(&events, &[]);
        assert!(matches!(restored, Some(None)));
    }

    #[test]
    fn no_history_and_empty_current_projects_nothing() {
        let s = session();
        let mut projection = RuntimeContextProjection::new(&cordis_rslite::Context::new(), &s);
        assert!(projection.project("", Vec::new()).is_none());
    }

    #[test]
    fn empty_current_produces_cleared_marker() {
        // A prior snapshot exists in history; clearing it must emit CLEARED.
        let mut s = session();
        let msg = Message {
            id: starriver_kernel::llm::MessageId("m0".into()),
            role: MessageRole::User,
            content: vec![starriver_kernel::llm::ContentBlock::Text("old".into())],
            source: MessageSource::Plugin {
                plugin: CONTEXT_PLUGIN.into(),
                form: None,
            },
        };
        let data = msg.to_json().unwrap();
        let _ = s.append_with(
            "user/message",
            data,
            starriver_kernel::session::AppendOptions {
                surface_op: Some(SurfaceOp::Append),
                source_event_seqs: Vec::new(),
            },
        );
        let mut projection = RuntimeContextProjection::new(&cordis_rslite::Context::new(), &s);
        let message = projection.project("", Vec::new()).unwrap();
        let text = match &message.content[0] {
            starriver_kernel::llm::ContentBlock::Text(t) => t.clone(),
            _ => String::new(),
        };
        assert!(text.contains(CLEARED));
    }
}
