//! Unified job panel: currently-running tasks + recent run history (
//! `jobs` parity; `running` / `completed` / `failed` vocabulary).

use crate::{types, HostRuntime};

impl HostRuntime {
    /// Merge active runs (running) with recent history (completed/failed),
    /// newest first, capped for the panel.
    pub fn list_jobs(&self) -> Vec<types::JobInfo> {
        let scheduler = self.scheduler.clone();
        let active = scheduler.list_active_runs();
        let mut jobs: Vec<types::JobInfo> = active
            .into_iter()
            .map(|run| types::JobInfo {
                task_name: run.task_name,
                status: "running".into(),
                started_at: run.started_at,
                summary: String::new(),
                session_id: Some(run.session_id),
            })
            .collect();
        let handle = self.runtime_handle.clone();
        let runs = handle.block_on(async move { scheduler.list_all_runs().await });
        for (name, record) in runs {
            if jobs
                .iter()
                .any(|j| j.task_name == name && j.status == "running")
            {
                continue;
            }
            jobs.push(types::JobInfo {
                task_name: name,
                status: if record.status == "success" {
                    "completed".into()
                } else {
                    "failed".into()
                },
                started_at: record.timestamp,
                summary: record.summary,
                session_id: record.session_id,
            });
        }
        jobs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        jobs.truncate(50);
        jobs
    }
}
