//! Phase B: native project catalog, bounded session attachment management and
//! persisted automation scheduler; all mutations are acknowledged by Studio.
use crate::studio::Studio;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledJob {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub schedule: Value,
    pub enabled: bool,
    pub label: String,
    pub prompt: String,
    #[serde(default)]
    pub actor_agent_id: Option<String>,
    #[serde(default)]
    pub next_run_at: Option<String>,
    #[serde(default)]
    pub last_run_at: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Persist before dispatch so an interrupted run is visible after restart.
    #[serde(default)]
    pub last_attempt_at: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub utc_offset_minutes: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StageBStore {
    pub revision: u64,
    pub catalog: Value,
    pub assignments: HashMap<String, String>,
    pub jobs: Vec<ScheduledJob>,
}
impl Default for StageBStore {
    fn default() -> Self {
        Self {
            revision: 0,
            catalog: json!({"folders": [], "projects": []}),
            assignments: HashMap::new(),
            jobs: vec![],
        }
    }
}
impl StageBStore {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let value: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        validate_project_state(&value.catalog, &value.assignments)?;
        ensure!(value.jobs.len() <= 300, "too many automation jobs");
        Ok(value)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        let tmp = path.with_extension("json.new");
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        if let Some(parent) = path.parent() {
            // Directory flush helps the rename survive an abrupt host restart.
            let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
        }
        Ok(())
    }
}
fn validate_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
fn validate_project_state(catalog: &Value, assignments: &HashMap<String, String>) -> Result<()> {
    let folders = catalog
        .get("folders")
        .and_then(Value::as_array)
        .context("folders array required")?;
    let projects = catalog
        .get("projects")
        .and_then(Value::as_array)
        .context("projects array required")?;
    ensure!(
        folders.len() <= 500 && projects.len() <= 1000 && assignments.len() <= 5000,
        "project catalog too large"
    );
    let mut folder_ids = HashSet::new();
    for row in folders {
        let id = row
            .get("id")
            .and_then(Value::as_str)
            .context("folder ID missing")?;
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .context("folder name missing")?;
        ensure!(
            validate_id(id)
                && !name.trim().is_empty()
                && name.len() <= 160
                && folder_ids.insert(id),
            "invalid folder"
        );
    }
    let mut project_ids = HashSet::new();
    for row in projects {
        let id = row
            .get("id")
            .and_then(Value::as_str)
            .context("project ID missing")?;
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .context("project name missing")?;
        ensure!(
            validate_id(id)
                && !name.trim().is_empty()
                && name.len() <= 160
                && project_ids.insert(id),
            "invalid project"
        );
        if let Some(folder) = row.get("folderId").and_then(Value::as_str) {
            ensure!(
                folder_ids.contains(folder),
                "project references nonexistent folder"
            );
        }
        if let Some(workspace) = row.get("workspacePath").and_then(Value::as_str) {
            ensure!(
                workspace.len() <= 4096 && !workspace.contains('\0'),
                "invalid workspace path"
            );
        }
    }
    for (session, id) in assignments {
        ensure!(
            session.len() <= 4096 && !session.is_empty() && project_ids.contains(id.as_str()),
            "session assignment references nonexistent project"
        );
    }
    Ok(())
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn timestamp_iso(millis: i64) -> Result<String> {
    Ok(chrono::DateTime::from_timestamp_millis(millis)
        .context("invalid automation timestamp")?
        .to_rfc3339())
}
fn cron_field(pattern: &str, value: u32, min: u32, max: u32) -> bool {
    pattern.split(',').any(|part| {
        let (base, step) = match part.split_once('/') {
            Some((base, step)) => match step.parse::<u32>() {
                Ok(n) if n > 0 && n <= max + 1 => (base, n),
                _ => return false,
            },
            None => (part, 1),
        };
        let range = if base == "*" {
            Some((min, max))
        } else if let Some((a, b)) = base.split_once('-') {
            a.parse::<u32>().ok().zip(b.parse::<u32>().ok())
        } else {
            base.parse::<u32>().ok().map(|a| (a, a))
        };
        match range {
            Some((a, b)) if a >= min && b <= max && a <= b => {
                value >= a && value <= b && (value - a) % step == 0
            }
            _ => false,
        }
    })
}
#[cfg(test)]
fn next_job_time(kind: &str, schedule: &Value, after_ms: i64) -> Result<Option<i64>> {
    next_job_time_offset(kind, schedule, after_ms, 0)
}
fn next_job_time_offset(
    kind: &str,
    schedule: &Value,
    after_ms: i64,
    utc_offset_minutes: i32,
) -> Result<Option<i64>> {
    ensure!(
        (-720..=840).contains(&utc_offset_minutes),
        "invalid cron UTC offset"
    );
    match kind {
        "every" => {
            let interval = schedule
                .as_i64()
                .context("every schedule must be milliseconds")?;
            ensure!(
                (60_000..=31_536_000_000).contains(&interval),
                "every interval must be between one minute and one year"
            );
            Ok(Some(
                after_ms
                    .checked_add(interval)
                    .context("interval overflow")?,
            ))
        }
        "at" => {
            let at = if let Some(value) = schedule.as_i64() {
                value
            } else {
                chrono::DateTime::parse_from_rfc3339(
                    schedule.as_str().context("at requires RFC3339 timestamp")?,
                )?
                .timestamp_millis()
            };
            ensure!(at >= 0, "invalid scheduled time");
            Ok(if at > after_ms { Some(at) } else { None })
        }
        "cron" => {
            use chrono::{Datelike, Timelike};
            let spec = schedule.as_str().context("cron needs 5 UTC fields")?;
            let fields = spec.split_whitespace().collect::<Vec<_>>();
            ensure!(fields.len() == 5, "cron expects 5 fields in UTC");
            let valid = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
            for (pattern, (min, max)) in fields.iter().zip(valid) {
                ensure!(
                    (min..=max).any(|n| cron_field(pattern, n, min, max)),
                    "invalid cron field"
                );
            }
            let first = after_ms.div_euclid(60_000) * 60_000 + 60_000;
            for minutes in 0..527_040_i64 {
                let millis = first + minutes * 60_000;
                let dt = chrono::DateTime::from_timestamp_millis(millis)
                    .context("invalid cron timestamp")?;
                let dt = dt + chrono::Duration::minutes(utc_offset_minutes as i64);
                let weekday = dt.weekday().num_days_from_sunday();
                let dom_matches = cron_field(fields[2], dt.day(), 1, 31);
                let dow_matches = cron_field(fields[4], weekday, 0, 7)
                    || (weekday == 0 && cron_field(fields[4], 7, 0, 7));
                // Standard five-field cron: when both day-of-month and weekday
                // are restricted, either match is sufficient.
                let day_matches = if fields[2] != "*" && fields[4] != "*" {
                    dom_matches || dow_matches
                } else {
                    dom_matches && dow_matches
                };
                if cron_field(fields[0], dt.minute(), 0, 59)
                    && cron_field(fields[1], dt.hour(), 0, 23)
                    && cron_field(fields[3], dt.month(), 1, 12)
                    && day_matches
                {
                    return Ok(Some(millis));
                }
            }
            bail!("cron schedule has no occurrence in the next year")
        }
        _ => bail!("unsupported automation schedule type"),
    }
}
/// A one-job execution lease. Released on success, failure, cancellation or panic.
struct AutomationPermit {
    studio: Studio,
    id: String,
}
impl Drop for AutomationPermit {
    fn drop(&mut self) {
        self.studio
            .shared
            .running_automations
            .lock()
            .unwrap()
            .remove(&self.id);
    }
}
impl Studio {
    fn acquire_automation(&self, id: &str) -> Result<AutomationPermit> {
        let mut running = self.shared.running_automations.lock().unwrap();
        ensure!(
            running.insert(id.to_owned()),
            "automation is already running"
        );
        Ok(AutomationPermit {
            studio: self.clone(),
            id: id.into(),
        })
    }
    pub fn project_catalog(&self) -> Value {
        let guard = self.shared.stage_b.lock().unwrap();
        json!({"ok":true,"revision":format!("r{}",guard.revision),
            "catalog":guard.catalog,"assignments":guard.assignments})
    }
    pub fn put_project_catalog(
        &self,
        revision: &str,
        catalog: Value,
        assignments: HashMap<String, String>,
    ) -> Result<Value> {
        validate_project_state(&catalog, &assignments)?;
        let mut guard = self.shared.stage_b.lock().unwrap();
        ensure!(
            revision == format!("r{}", guard.revision),
            "project revision conflict"
        );
        let mut next = guard.clone();
        next.revision += 1;
        next.catalog = catalog;
        next.assignments = assignments;
        next.save(&self.shared.stage_b_path)?;
        *guard = next;
        Ok(json!({"ok":true,"revision":format!("r{}",guard.revision),
            "catalog":guard.catalog,"assignments":guard.assignments}))
    }
    pub fn automation_jobs(&self) -> Value {
        let state = self.shared.stage_b.lock().unwrap();
        json!({"ok":true,"jobs":state.jobs,"schedulerAvailable":true,"editableDrafts":false})
    }
    pub fn mutate_automation(&self, payload: Value) -> Result<Value> {
        let action = payload
            .get("action")
            .and_then(Value::as_str)
            .context("automation action required")?;
        if action == "run" {
            bail!("use native run_automation_job for execution");
        }
        let mut state = self.shared.stage_b.lock().unwrap();
        let mut next = state.clone();
        let index = payload
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| next.jobs.iter().position(|row| row.id == id));
        let now = now_ms();
        let result = match action {
            "add" | "apply_suggestion" => {
                ensure!(next.jobs.len() < 300, "too many automation jobs");
                let item = if action == "apply_suggestion" {
                    let source = payload
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .context("source session required for automation suggestion")?;
                    let suggestion = payload
                        .get("suggestionId")
                        .and_then(Value::as_str)
                        .context("suggestion identity required")?;
                    ensure!(
                        validate_id(source)
                            && validate_id(suggestion)
                            && (self.session_on_disk(source) || self.agent(source).is_ok()),
                        "invalid automation suggestion session"
                    );
                    payload
                        .get("jobData")
                        .or_else(|| payload.get("job"))
                        .context("suggestion draft missing")?
                } else {
                    &payload
                };
                let id = loop {
                    let seq = self
                        .shared
                        .id_seq
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let candidate = format!("automation-{now}-{seq}");
                    if next.jobs.iter().all(|job| job.id != candidate) {
                        break candidate;
                    }
                };
                let mut job = ScheduledJob {
                    id: id.clone(),
                    kind: item
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("cron")
                        .into(),
                    schedule: item.get("schedule").cloned().unwrap_or(json!("0 9 * * *")),
                    enabled: item
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(action == "apply_suggestion"),
                    label: item
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    prompt: item
                        .get("prompt")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    actor_agent_id: item
                        .get("actorAgentId")
                        .or_else(|| item.get("targetAgentId"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    next_run_at: None,
                    last_run_at: None,
                    last_error: None,
                    last_attempt_at: None,
                    created_at: timestamp_iso(now)?,
                    utc_offset_minutes: item
                        .get("utcOffsetMinutes")
                        .or_else(|| payload.get("utcOffsetMinutes"))
                        .and_then(Value::as_i64)
                        .unwrap_or(0) as i32,
                };
                validate_job(&job)?;
                if job.enabled {
                    let next_time = next_job_time_offset(
                        &job.kind,
                        &job.schedule,
                        now,
                        job.utc_offset_minutes,
                    )?;
                    ensure!(next_time.is_some(), "expired one-shot cannot be enabled");
                    job.next_run_at = next_time.map(timestamp_iso).transpose()?;
                }
                next.jobs.push(job.clone());
                json!({"ok":true,"job":job})
            }
            "remove" => {
                let i = index.context("automation job not found")?;
                let removed = next.jobs.remove(i);
                json!({"ok":true,"removed":removed})
            }
            "toggle" | "update" => {
                let i = index.context("automation job not found")?;
                let job = &mut next.jobs[i];
                if action == "toggle" {
                    job.enabled = !job.enabled;
                } else {
                    if let Some(v) = payload.get("enabled").and_then(Value::as_bool) {
                        job.enabled = v;
                    }
                    if let Some(v) = payload.get("label").and_then(Value::as_str) {
                        job.label = v.into();
                    }
                    if let Some(v) = payload.get("prompt").and_then(Value::as_str) {
                        job.prompt = v.into();
                    }
                    if let Some(v) = payload.get("type").and_then(Value::as_str) {
                        job.kind = v.into();
                    }
                    if let Some(v) = payload.get("schedule") {
                        job.schedule = v.clone();
                    }
                    if let Some(v) = payload.get("actorAgentId") {
                        job.actor_agent_id = v.as_str().map(str::to_owned);
                    }
                }
                if let Some(offset) = payload.get("utcOffsetMinutes").and_then(Value::as_i64) {
                    ensure!((-720..=840).contains(&offset), "invalid UTC offset");
                    job.utc_offset_minutes = offset as i32;
                }
                validate_job(job)?;
                job.next_run_at = if job.enabled {
                    let next_time = next_job_time_offset(
                        &job.kind,
                        &job.schedule,
                        now,
                        job.utc_offset_minutes,
                    )?;
                    ensure!(next_time.is_some(), "expired one-shot cannot be enabled");
                    next_time.map(timestamp_iso).transpose()?
                } else {
                    None
                };
                json!({"ok":true,"job":job})
            }
            _ => bail!("unsupported automation action"),
        };
        next.save(&self.shared.stage_b_path)?;
        *state = next;
        Ok(result)
    }
    /// Send a real turn exactly once per invocation. The attempt is persisted
    /// before dispatch; success/error is persisted afterward. On process death
    /// a pending attempt remains explicitly marked interrupted, not "success".
    pub async fn run_automation_job(&self, id: &str) -> Result<Value> {
        let _lease = self.acquire_automation(id)?;
        let attempt = timestamp_iso(now_ms())?;
        let job = {
            let mut state = self.shared.stage_b.lock().unwrap();
            let mut next = state.clone();
            let row = next
                .jobs
                .iter_mut()
                .find(|job| job.id == id)
                .context("automation job not found")?;
            ensure!(!row.prompt.trim().is_empty(), "automation prompt required");
            row.last_attempt_at = Some(attempt);
            row.last_error = Some("execution interrupted before completion".into());
            let snapshot = row.clone();
            next.save(&self.shared.stage_b_path)?;
            *state = next;
            snapshot
        };
        let outcome: Result<String> = async {
            let agent_id = match job.actor_agent_id {
                Some(id) => id,
                None => {
                    // Persisted primary may be cold after an application restart.
                    let pinned = self.shared.agent_controls.lock().unwrap().primary.clone();
                    match pinned {
                        Some(id) if self.session_on_disk(&id) || self.agent(&id).is_ok() => id,
                        _ => self
                            .get_primary_agent()?
                            .get("agentId")
                            .and_then(Value::as_str)
                            .context("native primary Agent missing")?
                            .to_owned(),
                    }
                }
            };
            // Scheduled jobs may target a stored session. Resume it without
            // inventing a new Agent, but never duplicate an already-live one.
            if self.agent(&agent_id).is_err() {
                self.resume_session(&agent_id)?;
            }
            self.send_message(
                &agent_id,
                job.prompt,
                format!("scheduled-{}-{}", job.id, now_ms()),
            )
            .await?;
            Ok(agent_id)
        }
        .await;
        {
            let mut state = self.shared.stage_b.lock().unwrap();
            let mut next = state.clone();
            if let Some(row) = next.jobs.iter_mut().find(|row| row.id == id) {
                match &outcome {
                    Ok(_) => {
                        row.last_run_at = Some(timestamp_iso(now_ms())?);
                        row.last_error = None;
                    }
                    Err(error) => {
                        row.last_error = Some(error.to_string());
                    }
                }
                next.save(&self.shared.stage_b_path)?;
                *state = next;
            }
        }
        let agent_id = outcome?;
        Ok(json!({"ok":true,"status":"success","jobId":id,"agentId":agent_id}))
    }
    /// Atomically claim due jobs and advance their schedules before dispatch.
    /// An at-job has no future occurrence: execute once and disable it. Invalid
    /// schedules are quarantined independently and cannot stall other jobs.
    pub async fn tick_automations(&self) -> Result<usize> {
        let now = now_ms();
        let due = {
            let mut guard = self.shared.stage_b.lock().unwrap();
            let mut next = guard.clone();
            let mut ids = Vec::new();
            let mut changed = false;
            for job in &mut next.jobs {
                if !job.enabled {
                    continue;
                }
                let deadline = job
                    .next_run_at
                    .as_deref()
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                    .map(|dt| dt.timestamp_millis());
                let Some(deadline) = deadline else {
                    job.enabled = false;
                    job.last_error = Some("missing or malformed next run; disabled".into());
                    job.next_run_at = None;
                    changed = true;
                    continue;
                };
                if deadline > now {
                    continue;
                }
                // A running manual turn can overlap the clock. Defer without
                // advancing the due date, so it is still due at the next tick.
                if self
                    .shared
                    .running_automations
                    .lock()
                    .unwrap()
                    .contains(&job.id)
                {
                    continue;
                }
                match next_job_time_offset(&job.kind, &job.schedule, now, job.utc_offset_minutes) {
                    Ok(upcoming) => {
                        job.enabled = upcoming.is_some();
                        job.next_run_at = upcoming.map(timestamp_iso).transpose()?;
                        changed = true;
                        ids.push(job.id.clone());
                    }
                    Err(error) => {
                        job.enabled = false;
                        job.next_run_at = None;
                        job.last_error = Some(format!("invalid schedule: {error}"));
                        changed = true;
                    }
                }
            }
            if changed {
                next.save(&self.shared.stage_b_path)?;
                *guard = next;
            }
            ids
        };
        for id in &due {
            let studio = self.clone();
            let id = id.clone();
            tauri::async_runtime::spawn(async move {
                let _ = studio.run_automation_job(&id).await;
            });
        }
        Ok(due.len())
    }
    pub(crate) fn start_automation_scheduler(&self) {
        let studio = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(20));
            loop {
                interval.tick().await;
                if studio
                    .shared
                    .watch_stop
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    break;
                }
                let _ = studio.tick_automations().await;
            }
        });
    }
}
fn validate_job(job: &ScheduledJob) -> Result<()> {
    ensure!(
        (-720..=840).contains(&job.utc_offset_minutes),
        "invalid UTC offset"
    );
    ensure!(
        job.label.len() <= 160 && job.prompt.len() <= 65_536 && !job.prompt.trim().is_empty(),
        "invalid automation label/prompt"
    );
    if let Some(id) = &job.actor_agent_id {
        ensure!(validate_id(id), "invalid automation agent ID");
    }
    next_job_time_offset(&job.kind, &job.schedule, now_ms(), job.utc_offset_minutes)?;
    Ok(())
}

fn attachment_namespace(studio: &Studio, agent_id: &str) -> Result<std::path::PathBuf> {
    ensure!(validate_id(agent_id), "invalid attachment Agent ID");
    ensure!(
        studio.session_on_disk(agent_id) || studio.agent(agent_id).is_ok(),
        "session not found"
    );
    Ok(studio
        .shared
        .sessions_dir
        .parent()
        .context("sessions parent missing")?
        .join("session-files")
        .join(agent_id))
}
fn attachment_id_and_name(filename: &str) -> Option<(String, String)> {
    let segments = filename.splitn(5, '-').collect::<Vec<_>>();
    if segments.len() != 5
        || segments[0] != "studio"
        || segments[1] != "file"
        || segments[2].is_empty()
        || segments[3].is_empty()
        || segments[4].is_empty()
        || !segments[2].bytes().all(|b| b.is_ascii_hexdigit())
        || !segments[3].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    Some((
        format!("studio-file-{}-{}", segments[2], segments[3]),
        segments[4].to_owned(),
    ))
}
impl Studio {
    pub fn list_session_attachments(&self, id: &str) -> Result<Value> {
        let dir = attachment_namespace(self, id)?;
        let mut files = Vec::new();
        if dir.exists() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let metadata = std::fs::symlink_metadata(entry.path())?;
                if !metadata.is_file() || metadata.is_symlink() {
                    continue;
                }
                let filename = entry.file_name().to_string_lossy().to_string();
                if let Some((file_id, name)) = attachment_id_and_name(&filename) {
                    files.push(json!({"id":file_id,"name":name,"size":metadata.len(),
                        "sessionId":id,"stored":true}));
                }
            }
        }
        files.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        Ok(json!({"ok":true,"sessionId":id,"files":files}))
    }
    pub fn attachment_operation(&self, id: &str, file_id: &str, action: &str) -> Result<Value> {
        ensure!(
            validate_id(file_id) && file_id.starts_with("studio-file-"),
            "invalid attachment ID"
        );
        let dir = attachment_namespace(self, id)?;
        ensure!(dir.exists(), "no saved session attachments");
        let mut match_file = None;
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let filename = entry.file_name().to_string_lossy().to_string();
            if attachment_id_and_name(&filename)
                .as_ref()
                .is_some_and(|(candidate, _)| candidate == file_id)
            {
                ensure!(match_file.is_none(), "ambiguous attachment ID");
                match_file = Some(entry);
            }
        }
        let entry = match_file.context("attachment not found")?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.is_file() && !metadata.is_symlink() && metadata.len() <= 20 * 1024 * 1024,
            "unsafe or oversized attachment"
        );
        let canonical_dir = std::fs::canonicalize(&dir)?;
        ensure!(
            std::fs::canonicalize(entry.path())?.parent() == Some(canonical_dir.as_path()),
            "attachment path escaped managed cache"
        );
        let name = attachment_id_and_name(&entry.file_name().to_string_lossy())
            .context("invalid managed filename")?
            .1;
        match action {
            "read" => {
                use base64::Engine;
                let data = std::fs::read(entry.path())?;
                Ok(
                    json!({"ok":true,"sessionId":id,"id":file_id,"name":name,"size":data.len(),
                    "base64":base64::engine::general_purpose::STANDARD.encode(data)}),
                )
            }
            "delete" => {
                std::fs::remove_file(entry.path())?;
                Ok(json!({"ok":true,"sessionId":id,"id":file_id,"deleted":true}))
            }
            _ => bail!("unsupported attachment operation"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scheduler_supports_bounded_intervals_cron_utc_and_one_shot() {
        let origin = 1_800_000_000_000_i64;
        assert_eq!(
            next_job_time("every", &json!(60_000), origin).unwrap(),
            Some(origin + 60_000)
        );
        assert!(next_job_time("every", &json!(1_000), origin).is_err());
        assert!(next_job_time("every", &json!(31_536_000_001_i64), origin).is_err());
        assert!(
            next_job_time("cron", &json!("*/15 * * * *"), origin)
                .unwrap()
                .unwrap()
                > origin
        );
        assert!(next_job_time("cron", &json!("99 * * * *"), origin).is_err());
        assert_eq!(
            next_job_time("at", &json!(origin - 1), origin).unwrap(),
            None
        );
    }
    #[tokio::test]
    async fn native_project_cas_and_scheduler_are_persistent() {
        let path = std::env::temp_dir().join(format!("stage-b-project-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        let studio = Studio::with_hook(None, None, path.clone()).await.unwrap();
        let project = json!({"folders":[{"id":"f1","name":"Active","order":0}],
            "projects":[{"id":"p1","name":"Build","folderId":"f1","order":0,"workspacePath":"/tmp"}]});
        let mappings = HashMap::from([("studio://sess-1".into(), "p1".into())]);
        assert_eq!(studio.project_catalog()["revision"], "r0");
        let first = studio
            .put_project_catalog("r0", project.clone(), mappings.clone())
            .unwrap();
        assert_eq!(first["revision"], "r1");
        assert!(studio
            .put_project_catalog("r0", project.clone(), mappings.clone())
            .is_err());
        assert!(studio
            .put_project_catalog(
                "r1",
                project.clone(),
                HashMap::from([("x".into(), "missing".into())])
            )
            .is_err());
        let added = studio
            .mutate_automation(json!({"action":"add","type":"every", "schedule":60000,
            "prompt":"Perform local check", "label":"Check", "enabled":true}))
            .unwrap();
        assert!(added["job"]["nextRunAt"].is_string());
        let id = added["job"]["id"].as_str().unwrap();
        assert_eq!(
            studio
                .mutate_automation(json!({"action":"toggle","id":id}))
                .unwrap()["job"]["enabled"],
            false
        );
        assert!(studio.mutate_automation(json!({"action":"add","type":"at","schedule":"2000-01-01T00:00:00Z", "prompt":"Past", "enabled":true})).is_err());
        let stored = StageBStore::load(&path.join("stage-b.json")).unwrap();
        assert_eq!(stored.revision, 1);
        assert_eq!(stored.assignments.get("studio://sess-1").unwrap(), "p1");
        assert_eq!(stored.jobs.len(), 1);
        assert!(!stored.jobs[0].enabled);
        let _ = std::fs::remove_dir_all(path);
    }
    #[tokio::test]
    async fn phase_c_due_one_shots_and_invalid_jobs_do_not_stall_the_scheduler() {
        let path = std::env::temp_dir().join(format!("stage-c-due-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        let studio = Studio::with_hook(None, None, path.clone()).await.unwrap();
        let soon = timestamp_iso(now_ms() + 120_000).unwrap();
        let once = studio
            .mutate_automation(json!({"action":"add","type":"at",
            "schedule":soon,"prompt":"Send once","enabled":true}))
            .unwrap();
        let periodic = studio
            .mutate_automation(json!({"action":"add","type":"every",
            "schedule":60000,"prompt":"Keep checking","enabled":true}))
            .unwrap();
        let once_id = once["job"]["id"].as_str().unwrap().to_owned();
        let interval_id = periodic["job"]["id"].as_str().unwrap().to_owned();
        {
            let mut state = studio.shared.stage_b.lock().unwrap();
            state
                .jobs
                .iter_mut()
                .find(|row| row.id == once_id)
                .unwrap()
                .schedule = json!(timestamp_iso(now_ms() - 60_000).unwrap());
            state
                .jobs
                .iter_mut()
                .find(|row| row.id == once_id)
                .unwrap()
                .next_run_at = Some(timestamp_iso(now_ms() - 60_000).unwrap());
            state
                .jobs
                .iter_mut()
                .find(|row| row.id == interval_id)
                .unwrap()
                .next_run_at = Some(timestamp_iso(now_ms() - 60_000).unwrap());
            state.jobs.push(ScheduledJob {
                id: "invalid-stage-c".into(),
                kind: "cron".into(),
                schedule: json!("98 * * * *"),
                enabled: true,
                label: "bad".into(),
                prompt: "never".into(),
                actor_agent_id: None,
                next_run_at: Some(timestamp_iso(now_ms() - 60_000).unwrap()),
                last_run_at: None,
                last_error: None,
                last_attempt_at: None,
                created_at: timestamp_iso(now_ms()).unwrap(),
                utc_offset_minutes: 0,
            });
            state.save(&studio.shared.stage_b_path).unwrap();
        }
        assert_eq!(studio.tick_automations().await.unwrap(), 2);
        assert_eq!(studio.tick_automations().await.unwrap(), 0);
        let state = studio.shared.stage_b.lock().unwrap().clone();
        let at = state.jobs.iter().find(|row| row.id == once_id).unwrap();
        assert!(!at.enabled && at.next_run_at.is_none());
        let every = state.jobs.iter().find(|row| row.id == interval_id).unwrap();
        assert!(every.enabled && every.next_run_at.is_some());
        let bad = state
            .jobs
            .iter()
            .find(|row| row.id == "invalid-stage-c")
            .unwrap();
        assert!(
            !bad.enabled
                && bad
                    .last_error
                    .as_ref()
                    .unwrap()
                    .contains("invalid schedule")
        );
        // Reopen persisted state independently: due-once claim survives restart.
        let recovered = StageBStore::load(&studio.shared.stage_b_path).unwrap();
        assert!(
            !recovered
                .jobs
                .iter()
                .find(|row| row.id == once_id)
                .unwrap()
                .enabled
        );
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn phase_c_run_lease_and_failed_manual_dispatch_report_real_status() {
        let path = std::env::temp_dir().join(format!("stage-c-lease-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        let studio = Studio::with_hook(None, None, path.clone()).await.unwrap();
        let created = studio
            .mutate_automation(json!({"action":"add","type":"every",
            "schedule":60000,"prompt":"No primary Agent", "enabled":false}))
            .unwrap();
        let id = created["job"]["id"].as_str().unwrap();
        let lease = studio.acquire_automation(id).unwrap();
        assert!(studio
            .run_automation_job(id)
            .await
            .unwrap_err()
            .to_string()
            .contains("already running"));
        drop(lease);
        assert!(studio.run_automation_job(id).await.is_err());
        let state = StageBStore::load(&studio.shared.stage_b_path).unwrap();
        let job = state.jobs.iter().find(|row| row.id == id).unwrap();
        assert!(
            job.last_run_at.is_none(),
            "failed dispatch is not a successful run"
        );
        assert!(job.last_attempt_at.is_some());
        assert!(job
            .last_error
            .as_ref()
            .is_some_and(|error| error.contains("primary Agent")));
        // A final file without a committed rename must never overwrite state.
        std::fs::write(
            studio.shared.stage_b_path.with_extension("json.new"),
            b"partial",
        )
        .unwrap();
        assert_eq!(
            StageBStore::load(&studio.shared.stage_b_path)
                .unwrap()
                .jobs
                .len(),
            1
        );
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn managed_attachments_restrict_identity_and_symlinks() {
        use base64::Engine;
        let path = std::env::temp_dir().join(format!("stage-b-files-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        let studio = Studio::with_hook(None, None, path.clone()).await.unwrap();
        let agent = studio
            .create_agent(
                Some("phase-b-files".into()),
                "mock".into(),
                "mock-1".into(),
                None,
            )
            .unwrap();
        let payload = base64::engine::general_purpose::STANDARD.encode("hello attachment");
        let saved = studio
            .upload_blob(Some(&agent), "note.txt", &payload, Some("text/plain"))
            .await
            .unwrap();
        let file_id = saved["fileId"].as_str().unwrap();
        let listed = studio.list_session_attachments(&agent).unwrap();
        assert_eq!(listed["files"].as_array().unwrap().len(), 1);
        let read = studio
            .attachment_operation(&agent, file_id, "read")
            .unwrap();
        assert_eq!(read["base64"], payload);
        assert!(studio
            .attachment_operation(&agent, "../../unsafe", "read")
            .is_err());
        assert!(studio
            .attachment_operation("unknown", file_id, "read")
            .is_err());
        assert!(studio
            .attachment_operation(&agent, file_id, "delete")
            .unwrap()["deleted"]
            .as_bool()
            .unwrap());
        assert!(studio
            .attachment_operation(&agent, file_id, "read")
            .is_err());
        let _ = studio.dispose_agent(&agent);
        let _ = std::fs::remove_dir_all(path);
    }
}
