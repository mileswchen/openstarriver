//! Runner probe chain: probe each candidate runner, take the first
//! available, and aggregate per-runner failure details when none is usable
//! (fail closed with one explanatory message).

/// One runner's availability probe result.
#[derive(Debug, Clone)]
pub struct Probe {
    pub runner: &'static str,
    pub available: bool,
    /// Empty when available; otherwise why the probe failed.
    pub detail: String,
}

/// First available runner in probe order, or every probe's failure detail
/// joined for the `SandboxError::Unavailable` message.
pub fn pick(probes: &[Probe]) -> Result<&'static str, String> {
    if let Some(p) = probes.iter().find(|p| p.available) {
        return Ok(p.runner);
    }
    let detail = probes
        .iter()
        .map(|p| format!("{}: {}", p.runner, p.detail))
        .collect::<Vec<_>>()
        .join("; ");
    Err(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_available_runner_wins_in_probe_order() {
        let probes = [
            Probe {
                runner: "bwrap",
                available: false,
                detail: "not found".into(),
            },
            Probe {
                runner: "landlock",
                available: true,
                detail: String::new(),
            },
            Probe {
                runner: "third",
                available: true,
                detail: String::new(),
            },
        ];
        assert_eq!(pick(&probes), Ok("landlock"));
    }

    #[test]
    fn all_probes_failed_aggregates_details() {
        let probes = [
            Probe {
                runner: "bwrap",
                available: false,
                detail: "uid map denied".into(),
            },
            Probe {
                runner: "landlock",
                available: false,
                detail: "ENOSYS".into(),
            },
        ];
        assert_eq!(
            pick(&probes),
            Err("bwrap: uid map denied; landlock: ENOSYS".to_string())
        );
    }
}
