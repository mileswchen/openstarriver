//! Scheduled task subsystem: structured frequency modes + validity window.
//! Each task is its own agent — no separate agent_name binding.
//! Persisted to ~/.starriver/scheduled_tasks.yaml

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

// ── Schedule Mode ──

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduleMode {
    /// Recurring on a calendar period
    Periodic {
        period: Period,
        /// Time of day: "HH:MM" (local)
        time: String,
        /// 0=Sun..6=Sat (only used when period = Weekly)
        #[serde(default)]
        day_of_week: Option<u8>,
        /// 1-31 (only used when period = Monthly)
        #[serde(default)]
        day_of_month: Option<u8>,
        /// 1-12 (only used when period = Yearly)
        #[serde(default)]
        month: Option<u8>,
    },
    /// Every N hours, optionally restricted to certain days of week
    Interval {
        /// 1-24
        hours: u32,
        /// 0=Sun..6=Sat; empty = every day
        #[serde(default)]
        days_of_week: Vec<u8>,
    },
    /// One-shot at a specific date+time (ISO 8601)
    Once { datetime: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    Daily,
    Weekly,
    Biweekly,
    Monthly,
    Quarterly,
    Yearly,
}

impl ScheduleMode {
    /// Human-readable description, e.g. "every Monday at 09:00"
    pub fn describe(&self) -> String {
        match self {
            ScheduleMode::Periodic {
                period,
                time,
                day_of_week,
                day_of_month,
                month,
            } => {
                let time_str = time;
                match period {
                    Period::Daily => format!("每天 {}", time_str),
                    Period::Weekly => {
                        let dow = day_of_week.map_or("?".into(), dow_cn);
                        format!("每{dow} {time_str}")
                    }
                    Period::Biweekly => format!("双周 {}", time_str),
                    Period::Monthly => {
                        let d = day_of_month.map_or("?".into(), |d| format!("{d}日"));
                        format!("每月{d} {time_str}")
                    }
                    Period::Quarterly => {
                        let d = day_of_month.map_or("?".into(), |d| format!("{d}日"));
                        let m = month.map_or("?".into(), |m| format!("{m}月"));
                        format!("每季度{m}{d} {time_str}")
                    }
                    Period::Yearly => {
                        let d = month.map_or("?".into(), |m| format!("{m}月"))
                            + &day_of_month.map_or(String::new(), |d| format!("{d}日"));
                        format!("每年{d} {time_str}")
                    }
                }
            }
            ScheduleMode::Interval {
                hours,
                days_of_week,
            } => {
                if days_of_week.is_empty() {
                    format!("每{hours}小时")
                } else {
                    let days: Vec<String> = days_of_week.iter().map(|d| dow_cn(*d)).collect();
                    format!("{} 每{hours}小时", days.join("、"))
                }
            }
            ScheduleMode::Once { datetime } => {
                // Short / odd-shaped datetimes must not panic the whole task
                // list (list_scheduled_tasks calls describe for every task);
                // fall back to the raw string instead of a blind slice.
                let head = datetime.get(..16).unwrap_or(datetime);
                format!("单次 {}", head.replace('T', " "))
            }
        }
    }
}

fn dow_cn(d: u8) -> String {
    match d % 7 {
        0 => "周日",
        1 => "周一",
        2 => "周二",
        3 => "周三",
        4 => "周四",
        5 => "周五",
        6 => "周六",
        _ => "?",
    }
    .into()
}

impl ScheduleMode {
    /// Field-domain validation (BUG-9): reject values that would silently
    /// produce an unparseable cron or fire on the wrong day *before* the
    /// task is persisted — `to_cron`/`compute_next_run` degrade to a silent
    /// None, so the entry points must be the ones that report.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            ScheduleMode::Periodic {
                time,
                day_of_week,
                day_of_month,
                month,
                ..
            } => {
                if parse_time(time).is_none() {
                    return Err(format!("time `{time}` 非法（应为 HH:MM，00:00–23:59）"));
                }
                if let Some(d) = day_of_week {
                    if *d > 6 {
                        return Err(format!("day_of_week {d} 非法（应为 0=周日…6=周六）"));
                    }
                }
                if let Some(d) = day_of_month {
                    if !(1..=31).contains(d) {
                        return Err(format!("day_of_month {d} 非法（应为 1–31）"));
                    }
                }
                if let Some(m) = month {
                    if !(1..=12).contains(m) {
                        return Err(format!("month {m} 非法（应为 1–12）"));
                    }
                }
            }
            ScheduleMode::Interval {
                hours,
                days_of_week,
            } => {
                if !(1..=24).contains(hours) {
                    return Err(format!("hours {hours} 非法（应为 1–24）"));
                }
                for d in days_of_week {
                    if *d > 6 {
                        return Err(format!("days_of_week {d} 非法（应为 0=周日…6=周六）"));
                    }
                }
            }
            ScheduleMode::Once { datetime } => {
                let ok = chrono::DateTime::parse_from_rfc3339(datetime).is_ok()
                    || chrono::NaiveDateTime::parse_from_str(datetime, "%Y-%m-%dT%H:%M").is_ok();
                if !ok {
                    return Err(format!(
                        "datetime `{datetime}` 非法（应为 RFC 3339 或 YYYY-MM-DDTHH:MM）"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Convert to a 6-field cron expression (sec min hour dom mon dow — the
    /// `cron` crate's format; a 5-field Unix cron does not parse there) for
    /// next-run computation.
    pub fn to_cron(&self) -> Option<String> {
        match self {
            ScheduleMode::Periodic {
                period,
                time,
                day_of_week,
                day_of_month,
                month,
            } => {
                let (h, m) = parse_time(time)?;
                match period {
                    Period::Daily => Some(format!("0 {m} {h} * * *")),
                    Period::Weekly => {
                        // The cron crate numbers weekdays 1=Sun..7=Sat; the
                        // task contract is 0=Sun..6=Sat. Shift (wrapping
                        // out-of-range values, matching describe's %7 leniency)
                        // or a weekday-pinned task fires on the wrong day and 0 never parses.
                        let dow = (*day_of_week)? % 7 + 1;
                        Some(format!("0 {m} {h} * * {dow}"))
                    }
                    Period::Biweekly => {
                        // No faithful cron equivalent (cron has no
                        // "every 14 days anchored at a date" form); emitting
                        // a daily cron here once masked as "runs every day".
                        // compute_next_run special-cases Biweekly.
                        None
                    }
                    Period::Monthly => {
                        let d = (*day_of_month)?;
                        Some(format!("0 {m} {h} {d} * *"))
                    }
                    Period::Quarterly => {
                        let m_base = month.unwrap_or(1);
                        let d = day_of_month.unwrap_or(1);
                        Some(format!(
                            "0 {m} {h} {d} {},{},{},{} *",
                            m_base,
                            (m_base + 3 - 1) % 12 + 1,
                            (m_base + 6 - 1) % 12 + 1,
                            (m_base + 9 - 1) % 12 + 1
                        ))
                    }
                    Period::Yearly => {
                        let mo = (*month)?;
                        let d = day_of_month.unwrap_or(1);
                        Some(format!("0 {m} {h} {d} {mo} *"))
                    }
                }
            }
            ScheduleMode::Interval {
                hours,
                days_of_week,
            } => {
                let h_expr = if *hours == 1 {
                    "*".into()
                } else {
                    format!("*/{}", hours)
                };
                if days_of_week.is_empty() {
                    Some(format!("0 0 {h_expr} * * *"))
                } else {
                    // Same 0=Sun..6=Sat → cron 1=Sun..7=Sat shift as Weekly.
                    let days: Vec<String> = days_of_week
                        .iter()
                        .map(|d| (d % 7 + 1).to_string())
                        .collect();
                    Some(format!("0 0 {h_expr} * * {}", days.join(",")))
                }
            }
            ScheduleMode::Once { datetime: _ } => {
                // No recurring cron — handled specially
                None
            }
        }
    }
}

fn parse_time(time: &str) -> Option<(u32, u32)> {
    let mut parts = time.splitn(2, ':');
    let h: u32 = parts.next().and_then(|s| s.parse().ok())?;
    let m: u32 = parts.next().and_then(|s| s.parse().ok())?;
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

// ── Task Types ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRunRecord {
    pub timestamp: String,
    pub status: String,
    pub summary: String,
    /// Session id under which the full transcript was persisted (SessionStore).
    /// None for legacy runs recorded before this field existed.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// One currently-running task (`jobs` running state parity).
#[derive(Debug, Clone)]
pub struct ActiveRun {
    pub task_name: String,
    pub started_at: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub name: String,
    /// Structured schedule mode
    pub mode: ScheduleMode,
    /// Task prompt — this IS the agent's system prompt
    pub prompt: String,
    /// Model key "providerId/variantId" the task runs under. Empty = active agent model.
    #[serde(default)]
    pub model: Option<String>,
    /// Agent the task runs as. `None`/empty = the built-in Chat Agent
    /// (legacy tasks predate the field).
    #[serde(default)]
    pub agent: Option<String>,
    /// Workspace the task's runs bind to. `None`/empty = ungrouped runs
    /// (they land under the sidebar Tasks section).
    #[serde(default)]
    pub workspace: Option<String>,
    pub enabled: bool,
    /// Start of validity window (YYYY-MM-DD), optional
    #[serde(default)]
    pub start_date: Option<String>,
    /// End of validity window (YYYY-MM-DD), optional
    #[serde(default)]
    pub end_date: Option<String>,
    /// Skill names whose bodies are injected into the run's system prompt
    /// (resolved from `/skill` tokens the user added to the prompt text).
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub last_run: Option<String>,
    #[serde(default)]
    pub next_run: Option<String>,
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub run_history: Vec<TaskRunRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTaskConfig {
    pub tasks: HashMap<String, ScheduledTask>,
}

// ── Scheduler ──

pub struct TaskScheduler {
    tasks: Arc<tokio::sync::Mutex<ScheduledTaskConfig>>,
    config_path: PathBuf,
    active: Arc<std::sync::Mutex<Vec<ActiveRun>>>,
}

impl TaskScheduler {
    pub fn new(config_path: PathBuf) -> Self {
        Self {
            tasks: Arc::new(tokio::sync::Mutex::new(ScheduledTaskConfig {
                tasks: HashMap::new(),
            })),
            config_path,
            active: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Mark one task as running (job panel "running" state).
    pub fn mark_running(&self, task_name: &str, session_id: &str) {
        let mut active = self.active.lock().unwrap();
        active.retain(|r| r.task_name != task_name);
        active.push(ActiveRun {
            task_name: task_name.to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
            session_id: session_id.to_string(),
        });
    }

    /// Clear the running marker for one task (called with record_run).
    pub fn mark_finished(&self, task_name: &str) {
        self.active
            .lock()
            .unwrap()
            .retain(|r| r.task_name != task_name);
    }

    /// Currently-running tasks.
    pub fn list_active_runs(&self) -> Vec<ActiveRun> {
        self.active.lock().unwrap().clone()
    }

    pub async fn load(&self) -> anyhow::Result<()> {
        if self.config_path.exists() {
            let content = tokio::fs::read_to_string(&self.config_path).await?;
            if content.trim().is_empty() {
                return Ok(());
            }
            match serde_yaml::from_str::<ScheduledTaskConfig>(&content) {
                Ok(config) => *self.tasks.lock().await = config,
                Err(error) => {
                    // A corrupt task file must not masquerade as empty — the
                    // next save() would irreversibly overwrite the user's
                    // whole task list. Keep a .corrupt backup (same policy as
                    // settings.yaml) and surface the error.
                    let backup = self
                        .config_path
                        .with_extension("yaml.corrupt");
                    let _ = tokio::fs::copy(&self.config_path, &backup).await;
                    tracing::error!(
                        path = %self.config_path.display(),
                        backup = %backup.display(),
                        %error,
                        "scheduled_tasks.yaml corrupt; starting empty (backup kept)"
                    );
                    return Err(anyhow::anyhow!(
                        "scheduled_tasks.yaml corrupt (backup at {}): {error}",
                        backup.display()
                    ));
                }
            }
        }
        Ok(())
    }

    pub async fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.config_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let guard = self.tasks.lock().await;
        let yaml = serde_yaml::to_string(&*guard)?;
        tokio::fs::write(&self.config_path, yaml).await?;
        Ok(())
    }

    pub async fn list(&self) -> Vec<ScheduledTask> {
        self.tasks.lock().await.tasks.values().cloned().collect()
    }

    pub async fn get(&self, name: &str) -> Option<ScheduledTask> {
        self.tasks.lock().await.tasks.get(name).cloned()
    }

    pub async fn add(&self, task: ScheduledTask) -> anyhow::Result<()> {
        self.tasks
            .lock()
            .await
            .tasks
            .insert(task.name.clone(), task);
        self.save().await
    }

    pub async fn remove(&self, name: &str) -> anyhow::Result<()> {
        self.tasks.lock().await.tasks.remove(name);
        self.save().await
    }

    pub async fn update(&self, name: &str, task: ScheduledTask) -> anyhow::Result<()> {
        // The guard MUST drop before save(): save re-locks `tasks` and the
        // tokio Mutex is not reentrant — holding it across the call
        // self-deadlocked every update (no test ever reached this path
        // before, so it shipped broken).
        {
            let mut guard = self.tasks.lock().await;
            if guard.tasks.contains_key(name) {
                guard.tasks.insert(name.to_string(), task);
            }
        }
        self.save().await
    }

    /// All run records across every task, newest-first, tagged with the task name.
    /// Powers the cross-task History view.
    pub async fn list_all_runs(&self) -> Vec<(String, TaskRunRecord)> {
        let guard = self.tasks.lock().await;
        let mut all: Vec<(String, TaskRunRecord)> = guard
            .tasks
            .iter()
            .flat_map(|(name, t)| t.run_history.iter().map(|r| (name.clone(), r.clone())))
            .collect();
        all.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
        all
    }

    /// Append a run record and refresh last_run/last_status (used by execution paths).
    pub async fn record_run(
        &self,
        name: &str,
        status: String,
        summary: String,
        session_id: Option<String>,
    ) -> anyhow::Result<()> {
        let mut guard = self.tasks.lock().await;
        if let Some(task) = guard.tasks.get_mut(name) {
            task.run_history.push(TaskRunRecord {
                timestamp: chrono::Utc::now().to_rfc3339(),
                status: status.clone(),
                summary,
                session_id,
            });
            if task.run_history.len() > 50 {
                task.run_history.drain(..task.run_history.len() - 50);
            }
            task.last_run = Some(chrono::Utc::now().to_rfc3339());
            task.last_status = Some(status);
        }
        drop(guard);
        self.save().await
    }

    /// Enabled tasks inside their validity window whose `next_run` is due
    /// (≤ now). Exactly the set the scheduler tick fires (BUG-5); tasks
    /// with a missing/unparseable `next_run` are NOT due — the tick's heal
    /// pass recomputes those.
    pub async fn due_tasks(&self) -> Vec<ScheduledTask> {
        let guard = self.tasks.lock().await;
        let now = chrono::Local::now();
        guard
            .tasks
            .values()
            .filter(|t| {
                t.enabled
                    && TaskScheduler::in_window(t)
                    && t.next_run
                        .as_deref()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .is_some_and(|dt| dt.with_timezone(&chrono::Local) <= now)
            })
            .cloned()
            .collect()
    }

    /// Enabled recurring tasks whose `next_run` is missing or unparseable —
    /// the tick recomputes and stores a fresh value for each (self-healing
    /// for tasks saved while next_run computation was broken, e.g. before
    /// the 6-field cron fix). Once tasks are excluded: a missing next_run on
    /// a Once task means it already fired (compute returned None).
    pub async fn stale_next_runs(&self) -> Vec<ScheduledTask> {
        let guard = self.tasks.lock().await;
        guard
            .tasks
            .values()
            .filter(|t| {
                t.enabled
                    && !matches!(t.mode, ScheduleMode::Once { .. })
                    && t.next_run
                        .as_deref()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .is_none()
            })
            .cloned()
            .collect()
    }

    /// Compute the next run time from a ScheduleMode + optional start_date.
    /// Calendar fields (`time`, start_date anchors, naive Once datetimes)
    /// are interpreted in the **local** timezone, matching the field docs;
    /// the result is an offset-aware RFC 3339 string. Returns None for Once
    /// tasks that have already passed (or whose datetime cannot be parsed).
    pub fn compute_next_run(mode: &ScheduleMode, start_date: Option<&str>) -> Option<String> {
        let now = chrono::Local::now();

        match mode {
            ScheduleMode::Once { datetime } => {
                let dt = chrono::DateTime::parse_from_rfc3339(datetime)
                    .ok()
                    .map(DateTimeExt::into_local)
                    .or_else(|| {
                        // Naive "YYYY-MM-DDTHH:MM" is local wall-clock time.
                        chrono::NaiveDateTime::parse_from_str(datetime, "%Y-%m-%dT%H:%M")
                            .ok()
                            .and_then(|naive| naive.and_local_timezone(chrono::Local).earliest())
                    })?;
                if dt > now {
                    Some(dt.to_rfc3339())
                } else {
                    None
                }
            }

            ScheduleMode::Periodic {
                period: Period::Biweekly,
                time,
                ..
            } => {
                let (h, m) = parse_time(time)?;
                let start = start_date
                    .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
                    .unwrap_or_else(|| now.date_naive());
                let target_naive = start.and_hms_opt(h, m, 0)?;
                let target = target_naive.and_local_timezone(chrono::Local).earliest()?;
                let diff = now.signed_duration_since(target);
                let days_since = diff.num_days();
                let intervals_passed = ((days_since as f64) / 14.0).ceil() as i64;
                let next = target + chrono::Duration::days(intervals_passed.max(0) * 14);
                if next > now {
                    Some(next.to_rfc3339())
                } else {
                    Some((next + chrono::Duration::days(14)).to_rfc3339())
                }
            }

            _ => {
                // Use cron-based computation
                let cron_expr = mode.to_cron()?;
                let schedule: cron::Schedule = cron_expr.parse().ok()?;
                schedule
                    .upcoming(chrono::Local)
                    .next()
                    .map(|dt| dt.to_rfc3339())
            }
        }
    }

    /// Check if a task is currently within its validity window (local
    /// calendar days).
    pub fn in_window(task: &ScheduledTask) -> bool {
        let today = chrono::Local::now().date_naive();
        if let Some(ref sd) = task.start_date {
            if let Ok(start) = chrono::NaiveDate::parse_from_str(sd, "%Y-%m-%d") {
                if today < start {
                    return false;
                }
            }
        }
        if let Some(ref ed) = task.end_date {
            if let Ok(end) = chrono::NaiveDate::parse_from_str(ed, "%Y-%m-%d") {
                if today > end {
                    return false;
                }
            }
        }
        true
    }
}

/// Small conversion helper so the Once arm can normalize RFC 3339 inputs of
/// any offset onto the local zone (all comparisons below run in Local).
trait DateTimeExt: Sized {
    fn into_local(self) -> chrono::DateTime<chrono::Local>;
}

impl DateTimeExt for chrono::DateTime<chrono::FixedOffset> {
    fn into_local(self) -> chrono::DateTime<chrono::Local> {
        self.with_timezone(&chrono::Local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(name: &str, model: Option<String>) -> ScheduledTask {
        ScheduledTask {
            name: name.into(),
            mode: ScheduleMode::Periodic {
                period: Period::Daily,
                time: "09:00".into(),
                day_of_week: None,
                day_of_month: None,
                month: None,
            },
            prompt: "do a thing".into(),
            model,
            agent: None,
            workspace: None,
            enabled: true,
            start_date: None,
            end_date: None,
            skills: vec![],
            last_run: None,
            next_run: None,
            last_status: None,
            run_history: vec![],
        }
    }

    #[test]
    fn model_field_roundtrips_through_yaml() {
        let t = task("review", Some("deepseek/deepseek-chat".into()));
        let yaml = serde_yaml::to_string(&t).unwrap();
        assert!(
            yaml.contains("model: deepseek/deepseek-chat"),
            "yaml:\n{yaml}"
        );
        let back: ScheduledTask = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.model.as_deref(), Some("deepseek/deepseek-chat"));
    }

    #[test]
    fn model_field_defaults_to_none_for_legacy_yaml() {
        // Old config files have no model key — must still deserialize.
        let legacy = serde_yaml::to_string(&task("legacy", None)).unwrap();
        let stripped = legacy
            .lines()
            .filter(|l| !l.trim_start().starts_with("model:"))
            .collect::<Vec<_>>()
            .join("\n");
        let back: ScheduledTask = serde_yaml::from_str(&stripped).unwrap();
        assert_eq!(back.model, None);
    }

    #[test]
    fn skills_field_roundtrips_through_yaml() {
        let mut t = task("review", None);
        t.skills = vec!["code-review".into(), "summarize".into()];
        let yaml = serde_yaml::to_string(&t).unwrap();
        assert!(yaml.contains("skills:"), "yaml:\n{yaml}");
        let back: ScheduledTask = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            back.skills,
            vec!["code-review".to_string(), "summarize".to_string()]
        );
    }

    #[test]
    fn skills_field_defaults_to_empty_for_legacy_yaml() {
        // Old config files predate the skills key — must deserialize to an empty vec.
        let legacy = serde_yaml::to_string(&task("legacy", None)).unwrap();
        let stripped = legacy
            .lines()
            .filter(|l| !l.trim_start().starts_with("skills:"))
            .collect::<Vec<_>>()
            .join("\n");
        let back: ScheduledTask = serde_yaml::from_str(&stripped).unwrap();
        assert!(back.skills.is_empty());
    }

    #[tokio::test]
    async fn record_run_appends_history_and_updates_last_status() {
        let dir = std::env::temp_dir().join(format!("sched_test_{}", std::process::id()));
        let sched = TaskScheduler::new(dir.join("sched.yaml"));
        sched.add(task("t", None)).await.unwrap();
        sched
            .record_run("t", "success".into(), "done".into(), Some("sess-1".into()))
            .await
            .unwrap();
        let t = sched.get("t").await.unwrap();
        assert_eq!(t.run_history.len(), 1);
        assert_eq!(t.run_history[0].session_id.as_deref(), Some("sess-1"));
        assert_eq!(t.last_status.as_deref(), Some("success"));
        assert!(t.last_run.is_some());

        // History is capped at 50 records.
        for i in 0..60 {
            sched
                .record_run("t", "success".into(), format!("run {i}"), None)
                .await
                .unwrap();
        }
        let t = sched.get("t").await.unwrap();
        assert_eq!(t.run_history.len(), 50);
        assert_eq!(t.run_history.last().unwrap().summary, "run 59");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_all_runs_flattens_and_orders_newest_first() {
        let dir = std::env::temp_dir().join(format!("sched_allruns_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sched = TaskScheduler::new(dir.join("sched.yaml"));
        sched.add(task("a", None)).await.unwrap();
        sched.add(task("b", None)).await.unwrap();
        // Use distinct, ordered timestamps so sort order is deterministic.
        sched
            .record_run("a", "success".into(), "a1".into(), Some("s1".into()))
            .await
            .unwrap();
        // Bump the clock by rewriting an older timestamp directly to simulate ordering.
        {
            let mut guard = sched.tasks.lock().await;
            guard.tasks.get_mut("a").unwrap().run_history[0].timestamp =
                "2026-01-01T00:00:00Z".to_string();
            guard
                .tasks
                .get_mut("b")
                .unwrap()
                .run_history
                .push(TaskRunRecord {
                    timestamp: "2026-02-01T00:00:00Z".to_string(),
                    status: "error".into(),
                    summary: "b1".into(),
                    session_id: Some("s2".into()),
                });
        }
        let all = sched.list_all_runs().await;
        assert_eq!(all.len(), 2);
        // Newest first → b1 (Feb) before a1 (Jan).
        assert_eq!(all[0].0, "b");
        assert_eq!(all[0].1.session_id.as_deref(), Some("s2"));
        assert_eq!(all[1].0, "a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_runs_mark_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let sched = TaskScheduler::new(dir.path().join("sched.yaml"));
        sched.mark_running("job1", "s1");
        sched.mark_running("job2", "s2");
        let active = sched.list_active_runs();
        assert_eq!(active.len(), 2);
        assert!(active.iter().any(|r| r.task_name == "job1"));

        // Re-marking the same task replaces its running record.
        sched.mark_running("job1", "s1b");
        let active = sched.list_active_runs();
        assert_eq!(active.iter().filter(|r| r.task_name == "job1").count(), 1);

        sched.mark_finished("job1");
        let active = sched.list_active_runs();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].task_name, "job2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── describe ──

    #[test]
    fn describe_covers_every_mode_branch() {
        let periodic =
            |period: Period, time: &str, dow: Option<u8>, dom: Option<u8>, month: Option<u8>| {
                ScheduleMode::Periodic {
                    period,
                    time: time.into(),
                    day_of_week: dow,
                    day_of_month: dom,
                    month,
                }
            };
        assert_eq!(
            periodic(Period::Daily, "09:00", None, None, None).describe(),
            "每天 09:00"
        );
        assert_eq!(
            periodic(Period::Weekly, "09:00", Some(1), None, None).describe(),
            "每周一 09:00"
        );
        // Missing optional fields degrade to "?" instead of panicking.
        assert_eq!(
            periodic(Period::Weekly, "09:00", None, None, None).describe(),
            "每? 09:00"
        );
        // day_of_week wraps modulo 7 (8 = next Monday).
        assert_eq!(
            periodic(Period::Weekly, "09:00", Some(8), None, None).describe(),
            "每周一 09:00"
        );
        assert_eq!(
            periodic(Period::Biweekly, "09:00", None, None, None).describe(),
            "双周 09:00"
        );
        assert_eq!(
            periodic(Period::Monthly, "09:00", None, Some(15), None).describe(),
            "每月15日 09:00"
        );
        assert_eq!(
            periodic(Period::Quarterly, "09:00", None, Some(2), Some(1)).describe(),
            "每季度1月2日 09:00"
        );
        assert_eq!(
            periodic(Period::Yearly, "09:00", None, Some(21), Some(3)).describe(),
            "每年3月21日 09:00"
        );
        assert_eq!(
            ScheduleMode::Interval {
                hours: 2,
                days_of_week: vec![],
            }
            .describe(),
            "每2小时"
        );
        assert_eq!(
            ScheduleMode::Interval {
                hours: 2,
                days_of_week: vec![0, 2],
            }
            .describe(),
            "周日、周二 每2小时"
        );
        assert_eq!(
            ScheduleMode::Once {
                datetime: "2026-01-02T09:00:00Z".into(),
            }
            .describe(),
            "单次 2026-01-02 09:00"
        );
    }

    #[test]
    fn describe_short_once_datetime_does_not_panic() {
        // Regression: describe used to slice datetime[..16] blindly, so a
        // date-only or otherwise short Once datetime panicked — and with it
        // the whole list_scheduled_tasks projection.
        let out = ScheduleMode::Once {
            datetime: "2026-01-02".into(),
        }
        .describe();
        assert_eq!(out, "单次 2026-01-02");
    }

    // ── to_cron ──

    fn periodic(time: &str, dow: Option<u8>, dom: Option<u8>, month: Option<u8>) -> ScheduleMode {
        ScheduleMode::Periodic {
            period: Period::Daily,
            time: time.into(),
            day_of_week: dow,
            day_of_month: dom,
            month,
        }
    }

    #[test]
    fn to_cron_covers_every_periodic_branch() {
        let mode = |period: Period| ScheduleMode::Periodic {
            period,
            time: "09:30".into(),
            day_of_week: Some(1),
            day_of_month: Some(15),
            month: Some(2),
        };
        assert_eq!(
            mode(Period::Daily).to_cron().as_deref(),
            Some("0 30 9 * * *")
        );
        // Weekly maps contract dow (0=Sun..6=Sat) onto cron's 1=Sun..7=Sat:
        // Some(1) = Monday → cron 2.
        assert_eq!(
            mode(Period::Weekly).to_cron().as_deref(),
            Some("0 30 9 * * 2")
        );
        assert_eq!(
            mode(Period::Monthly).to_cron().as_deref(),
            Some("0 30 9 15 * *")
        );
        // Quarterly fans the base month out to all four quarters (Feb→2,5,8,11).
        assert_eq!(
            mode(Period::Quarterly).to_cron().as_deref(),
            Some("0 30 9 15 2,5,8,11 *")
        );
        assert_eq!(
            mode(Period::Yearly).to_cron().as_deref(),
            Some("0 30 9 15 2 *")
        );
        // Once has no recurring cron.
        assert_eq!(
            ScheduleMode::Once {
                datetime: "2026-01-02T09:00:00Z".into()
            }
            .to_cron(),
            None
        );
        // Biweekly has no faithful cron form (a daily cron once masqueraded
        // as one); compute_next_run special-cases it.
        assert_eq!(
            ScheduleMode::Periodic {
                period: Period::Biweekly,
                time: "09:30".into(),
                day_of_week: None,
                day_of_month: None,
                month: None,
            }
            .to_cron(),
            None
        );
    }

    #[test]
    fn to_cron_interval_hours_and_day_filters() {
        assert_eq!(
            ScheduleMode::Interval {
                hours: 1,
                days_of_week: vec![],
            }
            .to_cron()
            .as_deref(),
            Some("0 0 * * * *")
        );
        assert_eq!(
            ScheduleMode::Interval {
                hours: 3,
                days_of_week: vec![1, 2],
            }
            .to_cron()
            .as_deref(),
            Some("0 0 */3 * * 2,3")
        );
    }

    /// The cron crate numbers weekdays 1=Sun..7=Sat and refuses 0 outright —
    /// the task contract is 0=Sun..6=Sat, so the mapping must shift by one.
    #[test]
    fn to_cron_shifts_day_of_week_onto_cron_numbering() {
        let weekly = |dow: u8| ScheduleMode::Periodic {
            period: Period::Weekly,
            time: "09:00".into(),
            day_of_week: Some(dow),
            day_of_month: None,
            month: None,
        };
        assert_eq!(
            weekly(0).to_cron().as_deref(),
            Some("0 0 9 * * 1"),
            "周日→cron 1"
        );
        assert_eq!(
            weekly(1).to_cron().as_deref(),
            Some("0 0 9 * * 2"),
            "周一→cron 2"
        );
        assert_eq!(
            weekly(6).to_cron().as_deref(),
            Some("0 0 9 * * 7"),
            "周六→cron 7"
        );
        for dow in 0..=6u8 {
            let cron = weekly(dow).to_cron().unwrap();
            cron.parse::<cron::Schedule>()
                .unwrap_or_else(|e| panic!("dow {dow}: {cron} does not parse: {e}"));
        }
        let interval = ScheduleMode::Interval {
            hours: 6,
            days_of_week: vec![0, 6],
        }
        .to_cron()
        .unwrap();
        assert_eq!(interval, "0 0 */6 * * 1,7");
        interval
            .parse::<cron::Schedule>()
            .unwrap_or_else(|e| panic!("{interval} does not parse: {e}"));
    }

    /// Regression: to_cron used to emit 5-field Unix cron, which the `cron`
    /// crate refuses to parse — compute_next_run then silently returned None
    /// for every cron-based mode and the tasks never ran. Every expression
    /// this type produces must parse.
    #[test]
    fn to_cron_output_parses_with_the_cron_crate() {
        let modes = vec![
            ScheduleMode::Periodic {
                period: Period::Daily,
                time: "09:30".into(),
                day_of_week: None,
                day_of_month: None,
                month: None,
            },
            ScheduleMode::Periodic {
                period: Period::Weekly,
                time: "00:05".into(),
                day_of_week: Some(6),
                day_of_month: None,
                month: None,
            },
            ScheduleMode::Periodic {
                period: Period::Monthly,
                time: "23:59".into(),
                day_of_week: None,
                day_of_month: Some(31),
                month: None,
            },
            ScheduleMode::Periodic {
                period: Period::Quarterly,
                time: "08:00".into(),
                day_of_week: None,
                day_of_month: Some(1),
                month: Some(1),
            },
            ScheduleMode::Periodic {
                period: Period::Yearly,
                time: "12:00".into(),
                day_of_week: None,
                day_of_month: Some(21),
                month: Some(12),
            },
            ScheduleMode::Interval {
                hours: 1,
                days_of_week: vec![],
            },
            ScheduleMode::Interval {
                hours: 6,
                days_of_week: vec![0, 6],
            },
        ];
        for mode in &modes {
            let cron = mode.to_cron().unwrap_or_else(|| panic!("{mode:?}"));
            cron.parse::<cron::Schedule>()
                .unwrap_or_else(|e| panic!("unparseable cron {cron:?} from {mode:?}: {e}"));
        }
    }

    #[test]
    fn to_cron_rejects_missing_fields_and_bad_times() {
        // Optional fields the chosen period requires are absent → no cron.
        let weekly = ScheduleMode::Periodic {
            period: Period::Weekly,
            time: "09:30".into(),
            day_of_week: None,
            day_of_month: None,
            month: None,
        };
        assert_eq!(weekly.to_cron(), None);
        let monthly = ScheduleMode::Periodic {
            period: Period::Monthly,
            time: "09:30".into(),
            day_of_week: None,
            day_of_month: None,
            month: None,
        };
        assert_eq!(monthly.to_cron(), None);
        // Out-of-range times are rejected before formatting.
        for bad in ["24:00", "09:60", "aa:bb", "0930", ""] {
            assert_eq!(periodic(bad, None, None, None).to_cron(), None, "{bad:?}");
        }
    }

    #[test]
    fn to_cron_interval_zero_hours_is_not_a_valid_schedule() {
        // Documents a validation gap: hours=0 yields a step the cron crate
        // refuses to parse, so compute_next_run silently returns None — the
        // task never runs and nothing reports why.
        let cron = ScheduleMode::Interval {
            hours: 0,
            days_of_week: vec![],
        }
        .to_cron()
        .unwrap();
        assert_eq!(cron, "0 0 */0 * * *");
        assert!(cron.parse::<cron::Schedule>().is_err(), "{cron}");
        assert!(TaskScheduler::compute_next_run(
            &ScheduleMode::Interval {
                hours: 0,
                days_of_week: vec![],
            },
            None
        )
        .is_none());
    }

    // ── parse_time ──

    #[test]
    fn parse_time_accepts_and_rejects() {
        assert_eq!(parse_time("09:30"), Some((9, 30)));
        assert_eq!(parse_time("9:5"), Some((9, 5)));
        assert_eq!(parse_time("23:59"), Some((23, 59)));
        assert_eq!(parse_time("24:00"), None);
        assert_eq!(parse_time("09:60"), None);
        assert_eq!(parse_time("abc"), None);
        assert_eq!(parse_time("09"), None);
    }

    // ── compute_next_run ──

    #[test]
    fn compute_next_run_once_future_and_past() {
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let next = TaskScheduler::compute_next_run(
            &ScheduleMode::Once {
                datetime: future.clone(),
            },
            None,
        )
        .unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(&next).unwrap() > chrono::Utc::now(),
            "{next}"
        );

        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        assert_eq!(
            TaskScheduler::compute_next_run(&ScheduleMode::Once { datetime: past }, None),
            None
        );

        // Naive "YYYY-MM-DDTHH:MM" (no zone) is local wall-clock time.
        let naive = (chrono::Local::now() + chrono::Duration::hours(2))
            .format("%Y-%m-%dT%H:%M")
            .to_string();
        assert!(
            TaskScheduler::compute_next_run(&ScheduleMode::Once { datetime: naive }, None)
                .is_some()
        );

        // Garbage datetimes yield None, not a panic.
        assert_eq!(
            TaskScheduler::compute_next_run(
                &ScheduleMode::Once {
                    datetime: "next tuesday".into(),
                },
                None
            ),
            None
        );
    }

    #[test]
    fn compute_next_run_daily_fires_at_configured_local_time() {
        use chrono::Timelike;
        // BUG-8 regression: the "HH:MM (local)" documented field used to be
        // interpreted against UTC; the next occurrence must land on the
        // configured wall-clock time in the local zone.
        let mode = periodic("09:00", None, None, None);
        let next = TaskScheduler::compute_next_run(&mode, None).unwrap();
        let dt = chrono::DateTime::parse_from_rfc3339(&next)
            .unwrap()
            .with_timezone(&chrono::Local);
        assert!(dt > chrono::Local::now(), "{next}");
        assert_eq!(
            (dt.hour(), dt.minute()),
            (9, 0),
            "fires at 09:00 local, got {next}"
        );
    }

    #[test]
    fn compute_next_run_biweekly_aligns_to_start_date_anchor() {
        let mode = ScheduleMode::Periodic {
            period: Period::Biweekly,
            time: "09:00".into(),
            day_of_week: None,
            day_of_month: None,
            month: None,
        };
        let next = TaskScheduler::compute_next_run(&mode, Some("2020-01-01")).unwrap();
        let dt = chrono::DateTime::parse_from_rfc3339(&next)
            .unwrap()
            .with_timezone(&chrono::Local);
        let anchor_naive = chrono::NaiveDate::parse_from_str("2020-01-01", "%Y-%m-%d")
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        let anchor = anchor_naive
            .and_local_timezone(chrono::Local)
            .earliest()
            .unwrap();
        let diff_days = dt.signed_duration_since(anchor).num_days();
        assert_eq!(
            diff_days % 14,
            0,
            "next must sit on a 14-day multiple of the anchor: {next}"
        );
        assert!(dt > chrono::Local::now(), "{next}");
        assert!(
            dt.signed_duration_since(chrono::Local::now()).num_days() <= 14,
            "next occurrence is at most one interval away: {next}"
        );
    }

    // ── in_window ──

    #[test]
    fn in_window_checks_start_and_end_dates() {
        let yesterday = (chrono::Local::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let tomorrow = (chrono::Local::now() + chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();

        let mut t = task("w", None);
        assert!(TaskScheduler::in_window(&t), "no window = always in window");

        t.start_date = Some(yesterday.clone());
        assert!(TaskScheduler::in_window(&t), "started yesterday → inside");

        t.start_date = Some(tomorrow.clone());
        assert!(!TaskScheduler::in_window(&t), "starts tomorrow → outside");

        t.start_date = None;
        t.end_date = Some(yesterday.clone());
        assert!(!TaskScheduler::in_window(&t), "ended yesterday → outside");

        t.end_date = Some(tomorrow.clone());
        assert!(TaskScheduler::in_window(&t), "ends tomorrow → inside");

        // Unparseable date strings are ignored rather than rejecting the task.
        t.end_date = Some("eventually".into());
        assert!(TaskScheduler::in_window(&t));
    }

    // ── validate (BUG-9) ──

    #[test]
    fn validate_accepts_well_formed_modes() {
        assert!(periodic("09:00", Some(0), Some(1), Some(1))
            .validate()
            .is_ok());
        assert!(periodic("23:59", None, None, None).validate().is_ok());
        assert!(ScheduleMode::Interval {
            hours: 1,
            days_of_week: vec![0, 6],
        }
        .validate()
        .is_ok());
        assert!(ScheduleMode::Interval {
            hours: 24,
            days_of_week: vec![],
        }
        .validate()
        .is_ok());
        assert!(ScheduleMode::Once {
            datetime: "2026-01-02T09:00:00Z".into(),
        }
        .validate()
        .is_ok());
        assert!(ScheduleMode::Once {
            datetime: "2026-01-02T09:00".into(),
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn validate_rejects_out_of_domain_fields() {
        for bad_time in ["24:00", "09:60", "nine"] {
            assert!(
                periodic(bad_time, None, None, None).validate().is_err(),
                "{bad_time}"
            );
        }
        assert!(periodic("09:00", Some(7), None, None).validate().is_err());
        assert!(periodic("09:00", None, Some(0), None).validate().is_err());
        assert!(periodic("09:00", None, Some(32), None).validate().is_err());
        assert!(periodic("09:00", None, None, Some(13)).validate().is_err());
        for bad_hours in [0u32, 25] {
            assert!(
                ScheduleMode::Interval {
                    hours: bad_hours,
                    days_of_week: vec![],
                }
                .validate()
                .is_err(),
                "hours={bad_hours}"
            );
        }
        assert!(ScheduleMode::Interval {
            hours: 3,
            days_of_week: vec![7],
        }
        .validate()
        .is_err());
        assert!(ScheduleMode::Once {
            datetime: "next tuesday".into(),
        }
        .validate()
        .is_err());
        // The error carries the offending field so the API surface can
        // surface it verbatim.
        let err = ScheduleMode::Interval {
            hours: 0,
            days_of_week: vec![],
        }
        .validate()
        .unwrap_err();
        assert!(err.contains("hours"), "{err}");
    }

    // ── due_tasks (scheduler tick scan, BUG-5) ──

    #[tokio::test]
    async fn due_tasks_selects_only_enabled_in_window_overdue() {
        let dir = tempfile::tempdir().unwrap();
        let sched = TaskScheduler::new(dir.path().join("sched.yaml"));

        let past = (chrono::Local::now() - chrono::Duration::minutes(5))
            .to_rfc3339()
            .to_string();
        let future = (chrono::Local::now() + chrono::Duration::hours(2))
            .to_rfc3339()
            .to_string();

        // Overdue + enabled + in window → due.
        let mut due = task("due", None);
        due.next_run = Some(past.clone());
        sched.add(due).await.unwrap();

        // Overdue but disabled → not due.
        let mut disabled = task("disabled", None);
        disabled.enabled = false;
        disabled.next_run = Some(past.clone());
        sched.add(disabled).await.unwrap();

        // Overdue but outside the validity window → not due.
        let mut expired = task("expired", None);
        expired.next_run = Some(past.clone());
        expired.end_date = Some(
            (chrono::Local::now() - chrono::Duration::days(1))
                .format("%Y-%m-%d")
                .to_string(),
        );
        sched.add(expired).await.unwrap();

        // Future next_run → not due.
        let mut future_task = task("future", None);
        future_task.next_run = Some(future);
        sched.add(future_task).await.unwrap();

        // Missing / unparseable next_run → not due (the tick's heal pass
        // recomputes those separately).
        let mut stale = task("stale", None);
        stale.next_run = None;
        sched.add(stale).await.unwrap();
        let mut garbage = task("garbage", None);
        garbage.next_run = Some("whenever".into());
        sched.add(garbage).await.unwrap();

        let due = sched.due_tasks().await;
        assert_eq!(
            due.len(),
            1,
            "{:?}",
            due.iter().map(|t| t.name.clone()).collect::<Vec<_>>()
        );
        assert_eq!(due[0].name, "due");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WI-164: a corrupt task file must fail the load loudly AND keep a
    /// `.corrupt` backup — otherwise every scheduled task silently dies
    /// and the next save would overwrite the user's task list.
    #[tokio::test]
    async fn corrupt_task_file_fails_load_and_keeps_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scheduled_tasks.yaml");
        std::fs::write(&path, "{{{{ not yaml: [unclosed").unwrap();
        let scheduler = TaskScheduler::new(path.clone());
        let error = scheduler
            .load()
            .await
            .expect_err("corrupt yaml must fail the load, not masquerade as empty");
        assert!(error.to_string().contains("corrupt"), "{error}");
        let backup = path.with_extension("yaml.corrupt");
        assert!(
            backup.exists(),
            "the corrupt file must be preserved for manual recovery"
        );
        // And the in-memory task list stays empty (nothing half-loaded).
        assert!(scheduler.tasks.lock().await.tasks.is_empty());
    }

}
