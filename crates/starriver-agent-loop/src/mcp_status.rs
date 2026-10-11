//! MCP availability notice (WI-163): digest-deduped pre-step projection of
//! the host's MCP server availability. When a configured server's tools are
//! NOT registered (discovery failed, supervision gave up), the model is told
//! so explicitly — silent capability loss had flash-tier models pattern-lock
//! into no-op filler loops while planning around tools that no longer existed
//! (2026-10-01 echo storm, session 98bcd4a5). Recovery is announced the same
//! way. The projection follows `time_context`'s shape: diff-suppressed,
//! restored across agent recreation from the session log.

use starriver_kernel::llm::{
    mint_message_id, ContentBlock, ContextForm, Message, MessageRole, MessageSource,
};
use starriver_kernel::session::Session;

/// Message-source tag used by the projection.
pub const MCP_STATUS_PLUGIN: &str = "mcp-status";
/// `ContextForm::Notice` summary markers distinguishing the two variants;
/// restore reads them back instead of parsing prose. The down summary
/// carries the server set (`mcp-unavailable:a,b`) so recovery bookkeeping
/// survives agent recreation without text parsing.
pub const FORM_DOWN: &str = "mcp-unavailable";
pub const FORM_UP: &str = "mcp-recovered";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Announced {
    /// Last notice listed these servers as unavailable.
    Down { servers: Vec<String> },
    /// Last notice announced recovery (of the preceding Down set).
    Up,
}

/// Change-only projector over the host feed. `project` fires when the SET of
/// unavailable servers changes — details alone never re-announce (flapping
/// error strings would spam every step), and steady states (healthy from the
/// start, or still-down with the same set) stay silent.
pub struct McpStatusProjection {
    last: Option<Announced>,
}

impl McpStatusProjection {
    pub fn new(session: &Session) -> Self {
        Self {
            last: restore(session),
        }
    }

    pub fn project(&mut self, unavailable: &[(String, String)]) -> Option<Message> {
        let next = if unavailable.is_empty() {
            match &self.last {
                Some(Announced::Down { .. }) => Announced::Up,
                // Healthy from the start, or recovery already announced.
                _ => return None,
            }
        } else {
            let mut servers: Vec<String> =
                unavailable.iter().map(|(server, _)| server.clone()).collect();
            servers.sort();
            servers.dedup();
            Announced::Down { servers }
        };
        if self.last.as_ref() == Some(&next) {
            return None;
        }
        self.last = Some(next.clone());
        let (text, form) = match &next {
            Announced::Down { servers } => {
                let list = unavailable
                    .iter()
                    .map(|(server, detail)| format!("- {server}: {detail}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let names = servers.join("、");
                (
                    format!(
                        "[mcp] 以下 MCP 服务器当前不可用,其工具在本会话中不存在:\n{list}\n请勿调用这些服务器的工具,也不要用其他工具模拟其功能(例如用 echo/bash 代替工单操作);相关请求会自动重试,恢复后会再次通知。若用户追问,可如实告知以上失败原因。当前不可用:{names}。"
                    ),
                    ContextForm::Notice {
                        summary: format!("{FORM_DOWN}:{}", servers.join(",")),
                    },
                )
            }
            Announced::Up => (
                "[mcp] 之前不可用的 MCP 服务器已恢复,其工具已重新注册、可以正常调用。".to_string(),
                ContextForm::Notice {
                    summary: FORM_UP.to_string(),
                },
            ),
        };
        Some(Message {
            id: mint_message_id(),
            role: MessageRole::User,
            content: vec![ContentBlock::Text(text)],
            source: MessageSource::Plugin {
                plugin: MCP_STATUS_PLUGIN.into(),
                form: Some(form),
            },
        })
    }
}

/// Replay the last announcement from the session log so a recreated agent
/// (per-run construction, resume, process restart) does not re-announce a
/// state the session already heard. The Down variant's server set rides the
/// notice summary, so nothing parses prose.
fn restore(session: &Session) -> Option<Announced> {
    for event in session.events().iter().rev() {
        if event.event_type != "user/message" {
            continue;
        }
        let Ok(message) = Message::from_json(event.data.clone()) else {
            continue;
        };
        let form = match &message.source {
            MessageSource::Plugin { plugin, form }
                if plugin == MCP_STATUS_PLUGIN => form.as_ref(),
            _ => continue,
        };
        return Some(match form {
            Some(ContextForm::Notice { summary }) if summary == FORM_UP => Announced::Up,
            Some(ContextForm::Notice { summary }) => {
                let mut servers: Vec<String> = summary
                    .strip_prefix(FORM_DOWN)
                    .unwrap_or_default()
                    .trim_start_matches(':')
                    .split(',')
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
                    .collect();
                servers.sort();
                servers.dedup();
                Announced::Down { servers }
            }
            // Older/unknown shapes still count as an announced down state so
            // a spurious recovery notice cannot fire without a real
            // transition.
            _ => Announced::Down {
                servers: Vec::new(),
            },
        });
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
            Some(ContentBlock::Text(text)) => text.clone(),
            other => format!("{other:?}"),
        }
    }

    fn append(session: &Session, message: &Message) {
        session
            .append_with(
                "user/message",
                message.to_json().expect("message json"),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: Vec::new(),
                },
            )
            .expect("user/message append failed");
    }

    fn down(nexplan: &str) -> Vec<(String, String)> {
        vec![("nexplan".to_string(), nexplan.to_string())]
    }

    #[test]
    fn healthy_from_the_start_stays_silent() {
        let mut projection = McpStatusProjection::new(&fresh());
        assert!(projection.project(&[]).is_none());
    }

    #[test]
    fn first_failure_announces_with_detail_and_guidance() {
        let mut projection = McpStatusProjection::new(&fresh());
        let message = projection
            .project(&down("MCP http request failed: connection refused"))
            .expect("first failure announces");
        let text = text_of(&message);
        assert!(text.contains("- nexplan: MCP http request failed"), "{text}");
        assert!(text.contains("不要用其他工具模拟"), "{text}");
        assert!(matches!(
            &message.source,
            MessageSource::Plugin { plugin, form }
                if plugin == MCP_STATUS_PLUGIN
                    && matches!(form, Some(ContextForm::Notice { summary })
                        if summary == "mcp-unavailable:nexplan")
        ));
    }

    #[test]
    fn same_set_is_suppressed_but_set_changes_reannounce() {
        let mut projection = McpStatusProjection::new(&fresh());
        let _ = projection.project(&down("refused")).unwrap();
        // Same set, different detail: silent (flapping error text must not
        // spam).
        assert!(projection.project(&down("timeout")).is_none());
        // The set grows: announce again.
        let grown = vec![
            ("nexplan".to_string(), "refused".to_string()),
            ("devops".to_string(), "refused".to_string()),
        ];
        let message = projection.project(&grown).expect("set change announces");
        assert!(text_of(&message).contains("- devops:"));
    }

    #[test]
    fn recovery_announces_once_then_steady_state_silent() {
        let mut projection = McpStatusProjection::new(&fresh());
        let _ = projection.project(&down("refused")).unwrap();
        let recovered = projection.project(&[]).expect("recovery announces");
        let text = text_of(&recovered);
        assert!(text.contains("已恢复"), "{text}");
        assert!(matches!(
            &recovered.source,
            MessageSource::Plugin { form, .. }
                if matches!(form, Some(ContextForm::Notice { summary }) if summary == FORM_UP)
        ));
        // Still healthy: silent.
        assert!(projection.project(&[]).is_none());
    }

    #[test]
    fn restore_silences_a_known_state_and_keeps_recovery_reachable() {
        let session = fresh();
        let mut first = McpStatusProjection::new(&session);
        let message = first.project(&down("refused")).unwrap();
        append(&session, &message);
        // A recreated agent: the same still-down state must not re-announce…
        let mut resumed = McpStatusProjection::new(&session);
        assert!(resumed.project(&down("timeout")).is_none());
        // …but recovery still announces (restore kept the Down set).
        assert!(resumed.project(&[]).is_some());
    }

    #[test]
    fn restore_of_an_up_notice_stays_silent() {
        let session = fresh();
        let mut first = McpStatusProjection::new(&session);
        append(&session, &first.project(&down("refused")).unwrap());
        append(&session, &first.project(&[]).unwrap());
        let mut resumed = McpStatusProjection::new(&session);
        assert!(resumed.project(&[]).is_none());
        // Down again after a restored Up: a fresh transition announces.
        assert!(resumed.project(&down("refused")).is_some());
    }

    #[test]
    fn restore_reads_the_server_set_from_the_notice_summary() {
        let session = fresh();
        let mut first = McpStatusProjection::new(&session);
        let message = first
            .project(&[
                ("b".to_string(), "err".to_string()),
                ("a".to_string(), "err".to_string()),
            ])
            .unwrap();
        append(&session, &message);
        let restored = restore(&session).expect("restored");
        assert_eq!(
            restored,
            Announced::Down {
                servers: vec!["a".to_string(), "b".to_string()]
            }
        );
    }

    #[test]
    fn restore_of_an_unknown_form_still_counts_as_announced() {
        // Defensive: a legacy/foreign shape must not reset the projector to
        // "never announced" (that would re-announce or, worse, allow a
        // spurious recovery notice).
        let session = fresh();
        session
            .append_with(
                "user/message",
                Message {
                    id: mint_message_id(),
                    role: MessageRole::User,
                    content: vec![ContentBlock::Text("[mcp] 旧格式通告".into())],
                    source: MessageSource::Plugin {
                        plugin: MCP_STATUS_PLUGIN.into(),
                        form: None,
                    },
                }
                .to_json()
                .unwrap(),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: Vec::new(),
                },
            )
            .unwrap();
        let mut resumed = McpStatusProjection::new(&session);
        assert!(resumed.project(&down("refused")).is_some(), "set change announces");
    }
}
