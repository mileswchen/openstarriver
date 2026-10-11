/// Agent system layered error model
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("Inference timeout: engine={engine}, elapsed={elapsed:?}")]
    InferenceTimeout {
        engine: String,
        elapsed: std::time::Duration,
    },
    #[error("Inference rate-limited: engine={engine}, retry_after={retry_after:?}")]
    InferenceRateLimit {
        engine: String,
        retry_after: std::time::Duration,
    },
    #[error("Model not found: engine={engine}, model={model}")]
    InferenceModelNotFound { engine: String, model: String },
    #[error("Inference engine unavailable: {0}")]
    InferenceUnavailable(String),
    #[error("Inference error: {0}")]
    InferenceError(String),

    #[error("Tool not found: {name}")]
    ToolNotFound { name: String },
    #[error("Tool execution failed: {name}, cause={cause}")]
    ToolExecutionFailed { name: String, cause: String },
    #[error("Tool permission denied: {name}, reason={reason}")]
    ToolPermissionDenied { name: String, reason: String },
    #[error("Tool timeout: {name}, timeout={timeout:?}")]
    ToolTimeout {
        name: String,
        timeout: std::time::Duration,
    },

    #[error("Session not found: {id}")]
    SessionNotFound { id: String },
    #[error("Session corrupted: {id}, detail={detail}")]
    SessionCorrupted { id: String, detail: String },

    #[error("Max turns reached: {turns}")]
    MaxTurnsReached { turns: usize },
    #[error("Context overflow: {tokens} tokens > {max} max")]
    ContextOverflow { tokens: usize, max: usize },
    #[error("Task cancelled")]
    Cancelled,
    #[error("no progress for {rounds} consecutive rounds, task stuck")]
    NoProgress { rounds: usize },
    #[error("Task timeout: {elapsed:?}")]
    TaskTimeout { elapsed: std::time::Duration },

    #[error("Approval timeout: session={session_id}")]
    ApprovalTimeout { session_id: String },

    #[error("Out of memory: available={available_ram_mb}MB, message={message}")]
    ResourceExhausted {
        available_ram_mb: usize,
        message: String,
    },
    #[error("Task queue full: max={max}")]
    TaskQueueFull { max: usize },

    #[error("Sandbox unavailable: {reason}")]
    SandboxUnavailable { reason: String },
    #[error("Configuration error: {0}")]
    ConfigError(String),
    #[error("Internal error: {0}")]
    InternalError(#[from] anyhow::Error),
}

impl AgentError {
    pub fn code(&self) -> u32 {
        match self {
            AgentError::InferenceTimeout { .. } => 1001,
            AgentError::InferenceRateLimit { .. } => 1002,
            AgentError::InferenceModelNotFound { .. } => 1003,
            AgentError::InferenceUnavailable(_) => 1004,
            AgentError::InferenceError(_) => 1005,
            AgentError::ToolNotFound { .. } => 2001,
            AgentError::ToolExecutionFailed { .. } => 2002,
            AgentError::ToolPermissionDenied { .. } => 2003,
            AgentError::ToolTimeout { .. } => 2004,
            AgentError::SessionNotFound { .. } => 3001,
            AgentError::SessionCorrupted { .. } => 3002,
            AgentError::MaxTurnsReached { .. } => 4001,
            AgentError::ContextOverflow { .. } => 4002,
            AgentError::Cancelled => 4003,
            AgentError::NoProgress { .. } => 4004,
            AgentError::TaskTimeout { .. } => 4005,
            AgentError::ApprovalTimeout { .. } => 4006,
            AgentError::ResourceExhausted { .. } => 5001,
            AgentError::TaskQueueFull { .. } => 5002,
            AgentError::SandboxUnavailable { .. } => 6001,
            AgentError::ConfigError(_) => 7001,
            AgentError::InternalError(_) => 9999,
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            AgentError::InferenceTimeout { .. }
                | AgentError::InferenceRateLimit { .. }
                | AgentError::ToolTimeout { .. }
                | AgentError::InferenceUnavailable(_)
                | AgentError::ToolExecutionFailed { .. }
        )
    }

    pub fn user_message(&self) -> String {
        self.to_string()
    }
}

impl From<rusqlite::Error> for AgentError {
    fn from(e: rusqlite::Error) -> Self {
        AgentError::InternalError(anyhow::anyhow!("database error: {e}"))
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(e: serde_json::Error) -> Self {
        AgentError::InternalError(anyhow::anyhow!("serialization error: {e}"))
    }
}

pub type AgentResult<T> = std::result::Result<T, AgentError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn error_codes_match_contract() {
        let cases: [(AgentError, u32); 21] = [
            (
                AgentError::InferenceTimeout {
                    engine: "e".into(),
                    elapsed: Duration::from_secs(1),
                },
                1001,
            ),
            (
                AgentError::InferenceRateLimit {
                    engine: "e".into(),
                    retry_after: Duration::from_secs(2),
                },
                1002,
            ),
            (
                AgentError::InferenceModelNotFound {
                    engine: "e".into(),
                    model: "m".into(),
                },
                1003,
            ),
            (AgentError::InferenceUnavailable("x".into()), 1004),
            (AgentError::InferenceError("x".into()), 1005),
            (AgentError::ToolNotFound { name: "n".into() }, 2001),
            (
                AgentError::ToolExecutionFailed {
                    name: "n".into(),
                    cause: "c".into(),
                },
                2002,
            ),
            (
                AgentError::ToolPermissionDenied {
                    name: "n".into(),
                    reason: "r".into(),
                },
                2003,
            ),
            (
                AgentError::ToolTimeout {
                    name: "n".into(),
                    timeout: Duration::from_secs(3),
                },
                2004,
            ),
            (AgentError::SessionNotFound { id: "i".into() }, 3001),
            (
                AgentError::SessionCorrupted {
                    id: "i".into(),
                    detail: "d".into(),
                },
                3002,
            ),
            (AgentError::MaxTurnsReached { turns: 5 }, 4001),
            (AgentError::ContextOverflow { tokens: 10, max: 8 }, 4002),
            (AgentError::Cancelled, 4003),
            (AgentError::NoProgress { rounds: 3 }, 4004),
            (
                AgentError::TaskTimeout {
                    elapsed: Duration::from_secs(1),
                },
                4005,
            ),
            (
                AgentError::ApprovalTimeout {
                    session_id: "s".into(),
                },
                4006,
            ),
            (
                AgentError::ResourceExhausted {
                    available_ram_mb: 64,
                    message: "m".into(),
                },
                5001,
            ),
            (AgentError::TaskQueueFull { max: 4 }, 5002),
            (AgentError::SandboxUnavailable { reason: "r".into() }, 6001),
            (AgentError::ConfigError("c".into()), 7001),
        ];
        for (err, code) in cases {
            assert_eq!(err.code(), code, "{err:?} expected code {code}");
        }
        assert_eq!(
            AgentError::InternalError(anyhow::anyhow!("boom")).code(),
            9999
        );
    }

    #[test]
    fn retryable_classification() {
        let yes = [
            AgentError::InferenceTimeout {
                engine: "e".into(),
                elapsed: Duration::from_secs(1),
            },
            AgentError::InferenceRateLimit {
                engine: "e".into(),
                retry_after: Duration::from_secs(2),
            },
            AgentError::ToolTimeout {
                name: "n".into(),
                timeout: Duration::from_secs(1),
            },
            AgentError::InferenceUnavailable("x".into()),
            AgentError::ToolExecutionFailed {
                name: "n".into(),
                cause: "c".into(),
            },
        ];
        for e in yes {
            assert!(e.is_retryable(), "{e:?} should be retryable");
        }
        let no = [
            AgentError::InferenceModelNotFound {
                engine: "e".into(),
                model: "m".into(),
            },
            AgentError::ToolNotFound { name: "n".into() },
            AgentError::Cancelled,
            AgentError::ConfigError("c".into()),
            AgentError::NoProgress { rounds: 3 },
            AgentError::InternalError(anyhow::anyhow!("x")),
        ];
        for e in no {
            assert!(!e.is_retryable(), "{e:?} should not be retryable");
        }
    }

    #[test]
    fn user_message_equals_display() {
        let e = AgentError::ToolNotFound {
            name: "bash".into(),
        };
        assert_eq!(e.user_message(), e.to_string());
        assert!(e.user_message().contains("bash"));
    }

    #[test]
    fn from_rusqlite_error_wraps_as_internal() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (1)", []).unwrap();
        let db_err: rusqlite::Error = conn.execute("INSERT INTO t VALUES (1)", []).unwrap_err();
        let agent_err: AgentError = db_err.into();
        assert_eq!(agent_err.code(), 9999);
        assert!(agent_err.to_string().contains("database error"));
    }

    #[test]
    fn from_serde_json_error_wraps_as_internal() {
        let err: serde_json::Error =
            serde_json::from_str::<serde_json::Value>("{bad}").unwrap_err();
        let agent_err: AgentError = err.into();
        assert_eq!(agent_err.code(), 9999);
        assert!(agent_err.to_string().contains("serialization error"));
    }
}
