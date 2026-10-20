//! The cron scheduler (s12): prompts that start at a future local time.
//!
//! s11 lets a slow *command* leave the loop, but nothing remembers *when* work
//! should start. A [`CronJob`] pairs a 5-field cron expression with a prompt.
//! [`CronScheduler::poll`] compares the expression against the local clock and
//! marks matching jobs as due; the loop then appends a `[Scheduled] <prompt>`
//! user message and lets the model act on it.
//!
//! ```text
//!   .scheduled_tasks.json        poll(now)         take_due()
//!   cron:   0 9 * * *    ---->   due?    ---->  [Scheduled] run tests
//!   prompt: run tests                                  |
//!                                          acknowledge / restore
//! ```
//!
//! Delivery is at-least-once: a job is only acknowledged once the model has
//! accepted the prompt, so a failed call puts the job back in the queue.
//! `durable` jobs are written to `.scheduled_tasks.json` and survive a restart.

use std::collections::BTreeMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{Datelike, Local, NaiveDateTime, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::tools::{required_str, Tool, ToolContext, ToolRegistry};

/// File (relative to the workspace) holding the durable jobs.
pub const SCHEDULED_TASKS_FILE: &str = ".scheduled_tasks.json";

/// Tool names.
pub const SCHEDULE_CRON_TOOL_NAME: &str = "schedule_cron";
pub const LIST_CRONS_TOOL_NAME: &str = "list_crons";
pub const CANCEL_CRON_TOOL_NAME: &str = "cancel_cron";

/// System-prompt guidance for the cron tools.
pub const CRON_PROMPT: &str =
    "Use schedule_cron for work that should start at a future local time.";

/// JSON Schema pattern for a job id, shared by the tool schemas.
const CRON_ID_SCHEMA: &str = "^cron_[0-9a-f]{8}$";

/// A tool error for the cron system.
fn cron_error(message: impl Into<String>) -> Error {
    Error::Tool {
        tool: "cron".to_string(),
        message: message.into(),
    }
}

/// Whether `id` matches `cron_` followed by 8 lowercase hex digits.
pub fn is_valid_cron_id(id: &str) -> bool {
    let Some(hex) = id.strip_prefix("cron_") else {
        return false;
    };
    hex.len() == 8
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Allocate a fresh, effectively unique id (`cron_` + 8 hex chars).
fn allocate_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    format!("cron_{:08x}", hasher.finish() as u32)
}

/// Does one cron field match `value`?
///
/// Supported forms: `*`, `*/N`, `N`, `N-M`, and comma-separated lists of
/// those. A malformed field matches nothing rather than panicking, so a job
/// saved by an older build can never take the process down.
fn field_matches(field: &str, value: u32) -> bool {
    if field == "*" {
        return true;
    }
    if let Some(step) = field.strip_prefix("*/") {
        return match step.parse::<u32>() {
            Ok(step) if step > 0 => value % step == 0,
            _ => false,
        };
    }
    if field.contains(',') {
        return field
            .split(',')
            .any(|part| field_matches(part.trim(), value));
    }
    if let Some((start, end)) = field.split_once('-') {
        return match (start.parse::<u32>(), end.parse::<u32>()) {
            (Ok(start), Ok(end)) => start <= value && value <= end,
            _ => false,
        };
    }
    field.parse::<u32>().map(|n| n == value).unwrap_or(false)
}

/// The `%Y-%m-%d %H:%M` marker that keeps a job from firing twice a minute.
fn minute_marker(moment: NaiveDateTime) -> String {
    moment.format("%Y-%m-%d %H:%M").to_string()
}

/// Does `expression` match `moment` in local time?
///
/// Day-of-month and day-of-week combine with cron's OR rule: when both are
/// restricted, either one matching is enough.
pub fn cron_matches(expression: &str, moment: NaiveDateTime) -> bool {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    let (minute, hour, day, month, weekday) =
        (fields[0], fields[1], fields[2], fields[3], fields[4]);
    field_matches(minute, moment.minute())
        && field_matches(hour, moment.hour())
        && field_matches(month, moment.month())
        && day_matches(day, weekday, moment)
}

/// Cron's day rule, kept in one place because `cron_matches` and `next_match`
/// must agree on it: day-of-month and day-of-week are ORed when both are set,
/// and a `*` in either means the other one decides.
fn day_matches(day: &str, weekday: &str, moment: NaiveDateTime) -> bool {
    let by_day = field_matches(day, moment.day());
    let by_weekday = field_matches(weekday, moment.weekday().num_days_from_sunday());
    match (day == "*", weekday == "*") {
        (true, true) => true,
        (true, false) => by_weekday,
        (false, true) => by_day,
        (false, false) => by_day || by_weekday,
    }
}

/// The first whole minute strictly after `after` that `expression` matches.
///
/// Walks day by day and only the minutes of a day whose date fields can match,
/// so `0 0 29 2 *` -- a leap day, up to a few years out -- costs a day scan
/// rather than a minute-by-minute one, and a date that can never match (Feb 30)
/// gives up instead of looping forever. A malformed expression has no next
/// match either.
pub fn next_match(expression: &str, after: NaiveDateTime) -> Option<NaiveDateTime> {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    if fields.len() != 5 {
        return None;
    }
    let (minute, hour, day, month, weekday) =
        (fields[0], fields[1], fields[2], fields[3], fields[4]);

    // Four years is the whole of the ordinary leap cycle. A schedule whose only
    // match is further out than that (a century year that skips Feb 29) is
    // reported as having no next run rather than scanned for indefinitely.
    let last = after.checked_add_signed(chrono::Duration::days(366 * 4))?;
    let mut moment = (after + chrono::Duration::minutes(1))
        .with_second(0)?
        .with_nanosecond(0)?;
    while moment <= last {
        if !field_matches(month, moment.month()) || !day_matches(day, weekday, moment) {
            // Nothing in this day can match; step to the start of the next one.
            moment = moment.date().succ_opt()?.and_hms_opt(0, 0, 0)?;
            continue;
        }
        if field_matches(hour, moment.hour()) && field_matches(minute, moment.minute()) {
            return Some(moment);
        }
        moment += chrono::Duration::minutes(1);
    }
    None
}

/// Validate one field against its range.
fn validate_field(field: &str, minimum: u32, maximum: u32) -> Result<()> {
    if field == "*" {
        return Ok(());
    }
    if let Some(step) = field.strip_prefix("*/") {
        return match step.parse::<u32>() {
            Ok(step) if step > 0 => Ok(()),
            _ => Err(cron_error(format!("invalid step `{field}`"))),
        };
    }
    if field.contains(',') {
        for part in field.split(',') {
            validate_field(part.trim(), minimum, maximum)?;
        }
        return Ok(());
    }
    if let Some((start, end)) = field.split_once('-') {
        let (start, end) = match (start.parse::<u32>(), end.parse::<u32>()) {
            (Ok(start), Ok(end)) => (start, end),
            _ => return Err(cron_error(format!("invalid range `{field}`"))),
        };
        if start > end {
            return Err(cron_error(format!("range start exceeds end in `{field}`")));
        }
        if start < minimum || end > maximum {
            return Err(cron_error(format!(
                "range `{field}` is outside [{minimum}-{maximum}]"
            )));
        }
        return Ok(());
    }
    match field.parse::<u32>() {
        Ok(value) if (minimum..=maximum).contains(&value) => Ok(()),
        Ok(value) => Err(cron_error(format!(
            "value {value} is outside [{minimum}-{maximum}]"
        ))),
        Err(_) => Err(cron_error(format!("invalid field `{field}`"))),
    }
}

/// Validate a 5-field cron expression (minute hour day month weekday).
pub fn validate_cron(expression: &str) -> Result<()> {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(cron_error(format!(
            "expected 5 fields, got {}",
            fields.len()
        )));
    }
    let rules = [
        ("minute", 0, 59),
        ("hour", 0, 23),
        ("day-of-month", 1, 31),
        ("month", 1, 12),
        ("day-of-week", 0, 6),
    ];
    for (field, (name, minimum, maximum)) in fields.iter().zip(rules) {
        validate_field(field, minimum, maximum)
            .map_err(|error| cron_error(format!("{name}: {}", describe(&error))))?;
    }
    Ok(())
}

/// Unwrap the message of a tool error for a friendlier prefix.
fn describe(error: &Error) -> String {
    match error {
        Error::Tool { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// One scheduled prompt, persisted (when durable) to `.scheduled_tasks.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronJob {
    /// `cron_` plus 8 hex characters.
    pub id: String,
    /// 5-field cron expression in local time.
    pub cron: String,
    /// Prompt handed to the model when the job is due.
    pub prompt: String,
    /// Fire again on the next match instead of being removed.
    pub recurring: bool,
    /// Survive a process restart.
    pub durable: bool,
    /// Due but not yet accepted by the model.
    #[serde(default)]
    pub pending_delivery: bool,
    /// `%Y-%m-%d %H:%M` of the minute this job last fired in.
    #[serde(default)]
    pub last_fired: Option<String>,
}

#[derive(Default)]
struct State {
    jobs: BTreeMap<String, CronJob>,
    queue: Vec<String>,
}

struct Inner {
    state: Mutex<State>,
    path: PathBuf,
}

/// Owns the job table and the due queue. Cheap to clone (shared state).
#[derive(Clone)]
pub struct CronScheduler {
    inner: Arc<Inner>,
}

impl CronScheduler {
    /// A scheduler persisting to `<workspace>/.scheduled_tasks.json`.
    ///
    /// Construction touches nothing on disk; call [`CronScheduler::load`] to
    /// restore durable jobs.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self::with_path(workspace.into().join(SCHEDULED_TASKS_FILE))
    }

    /// A scheduler persisting to an explicit file.
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        CronScheduler {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                path: path.into(),
            }),
        }
    }

    /// The durable-jobs file this scheduler reads and writes.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// The current local time.
    pub fn now() -> NaiveDateTime {
        Local::now().naive_local()
    }

    /// Restore durable jobs from disk, returning how many were loaded.
    ///
    /// Jobs already waiting for delivery are re-queued, so a restart in the
    /// middle of the deliver/acknowledge window still delivers them.
    pub fn load(&self) -> Result<usize> {
        let payload = match fs::read_to_string(&self.inner.path) {
            Ok(payload) => payload,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let items: Vec<CronJob> = serde_json::from_str(&payload).map_err(|error| {
            cron_error(format!(
                "could not parse {}: {error}",
                self.inner.path.display()
            ))
        })?;

        let mut state = self.lock();
        let mut loaded = 0;
        for job in items {
            let usable = validate_cron(&job.cron).is_ok()
                && is_valid_cron_id(&job.id)
                && !job.prompt.trim().is_empty();
            if !usable {
                continue;
            }
            if job.pending_delivery {
                state.queue.push(job.id.clone());
            }
            state.jobs.insert(job.id.clone(), job);
            loaded += 1;
        }
        Ok(loaded)
    }

    /// Register a job. The expression is validated before anything is stored.
    pub fn schedule(
        &self,
        cron: &str,
        prompt: &str,
        recurring: bool,
        durable: bool,
    ) -> Result<CronJob> {
        validate_cron(cron)?;
        if prompt.trim().is_empty() {
            return Err(cron_error("prompt cannot be empty"));
        }
        let job = CronJob {
            id: allocate_id(),
            cron: cron.trim().to_string(),
            prompt: prompt.to_string(),
            recurring,
            durable,
            pending_delivery: false,
            last_fired: None,
        };

        let mut state = self.lock();
        state.jobs.insert(job.id.clone(), job.clone());
        if durable {
            if let Err(error) = self.save_locked(&state) {
                state.jobs.remove(&job.id);
                return Err(error);
            }
        }
        Ok(job)
    }

    /// Remove a job by id.
    pub fn cancel(&self, job_id: &str) -> Result<String> {
        let mut state = self.lock();
        let Some(job) = state.jobs.remove(job_id) else {
            return Err(cron_error(format!("job `{job_id}` not found")));
        };
        state.queue.retain(|queued| queued != job_id);
        if job.durable {
            if let Err(error) = self.save_locked(&state) {
                if job.pending_delivery {
                    state.queue.push(job.id.clone());
                }
                state.jobs.insert(job.id.clone(), job);
                return Err(error);
            }
        }
        Ok(format!("Cancelled {job_id}"))
    }

    /// Every job, in id order.
    pub fn jobs(&self) -> Vec<CronJob> {
        self.lock().jobs.values().cloned().collect()
    }

    /// One job by id.
    pub fn get(&self, job_id: &str) -> Option<CronJob> {
        self.lock().jobs.get(job_id).cloned()
    }

    /// Number of jobs waiting to be delivered.
    pub fn pending(&self) -> usize {
        self.lock().queue.len()
    }

    /// Whether any job is waiting to be delivered.
    pub fn has_due(&self) -> bool {
        !self.lock().queue.is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().expect("cron state poisoned")
    }

    /// Atomically write the durable jobs (temp file + rename).
    fn save_locked(&self, state: &State) -> Result<()> {
        let durable: Vec<&CronJob> = state.jobs.values().filter(|job| job.durable).collect();
        let payload = serde_json::to_string_pretty(&durable)?;
        let temporary = self.inner.path.with_extension("json.tmp");
        fs::write(&temporary, payload)?;
        fs::rename(&temporary, &self.inner.path)?;
        Ok(())
    }
}

/// The delivery half of the scheduler: poll, hand out, acknowledge, restore.
impl CronScheduler {
    /// Mark every job whose expression matches `moment` as due.
    ///
    /// A job is skipped when it is already queued or when it has already fired
    /// in the same minute, so a one-second ticker enqueues each match exactly
    /// once. Durable state is written immediately; if that write fails the
    /// in-memory change is rolled back and the error returned.
    pub fn poll(&self, moment: NaiveDateTime) -> Result<()> {
        let marker = minute_marker(moment);
        let due: Vec<String> = {
            let state = self.lock();
            state
                .jobs
                .values()
                .filter(|job| !job.pending_delivery)
                .filter(|job| job.last_fired.as_deref() != Some(marker.as_str()))
                .filter(|job| cron_matches(&job.cron, moment))
                .map(|job| job.id.clone())
                .collect()
        };

        for id in due {
            let mut state = self.lock();
            let durable = match state.jobs.get_mut(&id) {
                Some(job) => {
                    job.pending_delivery = true;
                    job.last_fired = Some(marker.clone());
                    job.durable
                }
                None => continue,
            };
            if durable {
                if let Err(error) = self.save_locked(&state) {
                    if let Some(job) = state.jobs.get_mut(&id) {
                        job.pending_delivery = false;
                        job.last_fired = None;
                    }
                    return Err(error);
                }
            }
            state.queue.push(id);
        }
        Ok(())
    }

    /// Drain the due queue, returning the jobs in delivery order.
    pub fn take_due(&self) -> Vec<CronJob> {
        let mut state = self.lock();
        let ids = std::mem::take(&mut state.queue);
        ids.iter()
            .filter_map(|id| state.jobs.get(id).cloned())
            .collect()
    }

    /// Mark delivered jobs handled: recurring jobs wait for the next match,
    /// one-shot jobs are removed. Durable changes are persisted.
    pub fn acknowledge(&self, delivered: &[CronJob]) -> Result<()> {
        let mut state = self.lock();
        let mut removed: Vec<CronJob> = Vec::new();
        let mut cleared: Vec<String> = Vec::new();
        for job in delivered {
            match state.jobs.get_mut(&job.id) {
                None => {}
                Some(current) if current.recurring => {
                    current.pending_delivery = false;
                    cleared.push(job.id.clone());
                }
                Some(_) => {
                    if let Some(previous) = state.jobs.remove(&job.id) {
                        removed.push(previous);
                    }
                }
            }
        }

        if delivered.iter().any(|job| job.durable) {
            if let Err(error) = self.save_locked(&state) {
                for job in removed {
                    state.queue.retain(|queued| *queued != job.id);
                    state.jobs.insert(job.id.clone(), job);
                }
                for id in cleared {
                    if let Some(job) = state.jobs.get_mut(&id) {
                        job.pending_delivery = true;
                    }
                }
                for job in delivered {
                    if state.jobs.contains_key(&job.id) && !state.queue.contains(&job.id) {
                        state.queue.push(job.id.clone());
                    }
                }
                return Err(error);
            }
        }
        Ok(())
    }

    /// Put jobs back after a failed turn so they are delivered again.
    pub fn restore(&self, delivered: &[CronJob]) {
        let mut state = self.lock();
        for job in delivered {
            if let Some(current) = state.jobs.get_mut(&job.id) {
                current.pending_delivery = true;
            }
            if state.jobs.contains_key(&job.id) && !state.queue.contains(&job.id) {
                state.queue.push(job.id.clone());
            }
        }
    }

    /// Human-readable listing, one job per line.
    pub fn render(&self) -> String {
        let jobs = self.jobs();
        if jobs.is_empty() {
            return "No cron jobs.".to_string();
        }
        jobs.iter()
            .map(|job| {
                let frequency = if job.recurring {
                    "recurring"
                } else {
                    "one-shot"
                };
                let storage = if job.durable { "durable" } else { "session" };
                let prompt: String = job.prompt.chars().take(60).collect();
                format!(
                    "{}: {} -> {prompt} [{frequency}, {storage}]",
                    job.id, job.cron
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Poll every second on a detached thread, so a job becomes due even while
    /// the agent sits idle. Delivery still happens at the next turn boundary.
    pub fn start_ticker(&self) {
        let scheduler = self.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            if let Err(error) = scheduler.poll(CronScheduler::now()) {
                eprintln!("[cron] poll failed: {error}");
            }
        });
    }
}

/// `schedule_cron`: register a job and return its runtime-generated id.
pub struct ScheduleCronTool {
    scheduler: CronScheduler,
}

impl ScheduleCronTool {
    /// Bind the tool to a scheduler.
    pub fn new(scheduler: CronScheduler) -> Self {
        ScheduleCronTool { scheduler }
    }
}

#[async_trait]
impl Tool for ScheduleCronTool {
    fn name(&self) -> &str {
        SCHEDULE_CRON_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Schedule a prompt with a 5-field cron expression (minute hour day month weekday). \
         Set recurring=false for a one-shot job, durable=false to keep it in memory only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cron": {
                    "type": "string",
                    "description": "5-field cron expression in local time."
                },
                "prompt": {
                    "type": "string",
                    "description": "Prompt to hand to the model when the job is due."
                },
                "recurring": {
                    "type": "boolean",
                    "description": "Repeat on every match (default true)."
                },
                "durable": {
                    "type": "boolean",
                    "description": "Survive a restart (default true)."
                }
            },
            "required": ["cron", "prompt"],
            "additionalProperties": false
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        let cron = required_str(&input, "cron")?;
        let prompt = required_str(&input, "prompt")?;
        let recurring = input
            .get("recurring")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let durable = input
            .get("durable")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let job = self.scheduler.schedule(cron, prompt, recurring, durable)?;
        Ok(format!(
            "Scheduled {}: {} -> {}",
            job.id, job.cron, job.prompt
        ))
    }
}

/// `list_crons`: one line per scheduled job.
pub struct ListCronsTool {
    scheduler: CronScheduler,
}

impl ListCronsTool {
    /// Bind the tool to a scheduler.
    pub fn new(scheduler: CronScheduler) -> Self {
        ListCronsTool { scheduler }
    }
}

#[async_trait]
impl Tool for ListCronsTool {
    fn name(&self) -> &str {
        LIST_CRONS_TOOL_NAME
    }

    fn description(&self) -> &str {
        "List scheduled cron jobs."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn call(&self, _ctx: &ToolContext, _input: Value) -> Result<String> {
        Ok(self.scheduler.render())
    }
}

/// `cancel_cron`: remove a job by id.
pub struct CancelCronTool {
    scheduler: CronScheduler,
}

impl CancelCronTool {
    /// Bind the tool to a scheduler.
    pub fn new(scheduler: CronScheduler) -> Self {
        CancelCronTool { scheduler }
    }
}

#[async_trait]
impl Tool for CancelCronTool {
    fn name(&self) -> &str {
        CANCEL_CRON_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Cancel a cron job by ID."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string", "pattern": CRON_ID_SCHEMA }
            },
            "required": ["job_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
        let job_id = required_str(&input, "job_id")?;
        self.scheduler.cancel(job_id)
    }
}

/// Register `schedule_cron`, `list_crons`, and `cancel_cron`.
pub fn register_cron_tools(registry: &mut ToolRegistry, scheduler: CronScheduler) {
    registry.register(ScheduleCronTool::new(scheduler.clone()));
    registry.register(ListCronsTool::new(scheduler.clone()));
    registry.register(CancelCronTool::new(scheduler));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;

    fn moment(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    /// A Monday, so weekday assertions are unambiguous.
    fn monday() -> NaiveDateTime {
        moment(2026, 10, 12, 9, 0)
    }

    fn scheduler() -> (tempfile::TempDir, CronScheduler) {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = CronScheduler::new(dir.path());
        (dir, scheduler)
    }

    #[test]
    fn ids_have_the_expected_shape() {
        for _ in 0..50 {
            assert!(is_valid_cron_id(&allocate_id()));
        }
        assert!(!is_valid_cron_id("cron_1234567"));
        assert!(is_valid_cron_id("cron_12345678"));
        assert!(!is_valid_cron_id("cron_ABCDEF12"));
        assert!(!is_valid_cron_id("task_12345678"));
    }

    #[test]
    fn validation_accepts_the_supported_forms_and_rejects_the_rest() {
        for good in [
            "* * * * *",
            "0 9 * * *",
            "*/5 * * * *",
            "0 9 * * 1-5",
            "0,30 8-18 * * 1,3,5",
            "15 3 1 1 0",
        ] {
            assert!(validate_cron(good).is_ok(), "{good}");
        }
        for bad in [
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 7",
            "*/0 * * * *",
            "*/x * * * *",
            "9-1 * * * *",
            "a * * * *",
        ] {
            assert!(validate_cron(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cron_matches_reads_every_field() {
        assert!(cron_matches("* * * * *", monday()));
        assert!(cron_matches("0 9 * * *", monday()));
        assert!(!cron_matches("1 9 * * *", monday()));
        assert!(cron_matches("*/5 * * * *", moment(2026, 10, 12, 9, 20)));
        assert!(!cron_matches("*/5 * * * *", moment(2026, 10, 12, 9, 21)));
        assert!(cron_matches("0 9 12 10 *", monday()));
        assert!(!cron_matches("0 9 13 10 *", monday()));
        assert!(!cron_matches("0 9 12 11 *", monday()));
        // Monday is weekday 1 in cron's 0=Sunday numbering.
        assert!(cron_matches("0 9 * * 1", monday()));
        assert!(!cron_matches("0 9 * * 2", monday()));
        assert!(!cron_matches("garbage", monday()));
    }

    #[test]
    fn weekday_and_day_of_month_use_the_or_rule() {
        // Both unrestricted: any day matches.
        assert!(cron_matches("0 9 * * *", monday()));
        // Only the weekday is restricted: it decides.
        assert!(!cron_matches("0 9 * * 6", monday()));
        // Only the day-of-month is restricted: it decides.
        assert!(!cron_matches("0 9 13 * *", monday()));
        // Both restricted: either matching is enough.
        assert!(cron_matches("0 9 13 * 1", monday()));
        assert!(cron_matches("0 9 12 * 6", monday()));
        assert!(!cron_matches("0 9 13 * 6", monday()));
    }

    #[test]
    fn next_match_finds_the_first_matching_minute_after_the_moment() {
        // 08:59 -> 09:00, and the answer is strictly after: 09:00 -> tomorrow.
        assert_eq!(
            next_match("0 9 * * *", moment(2026, 10, 12, 8, 59)),
            Some(moment(2026, 10, 12, 9, 0))
        );
        assert_eq!(
            next_match("0 9 * * *", moment(2026, 10, 12, 9, 0)),
            Some(moment(2026, 10, 13, 9, 0))
        );
        // A step expression is read inside the hour, not only at the top of it.
        assert_eq!(
            next_match("*/15 * * * *", moment(2026, 10, 12, 10, 1)),
            Some(moment(2026, 10, 12, 10, 15))
        );
        // Seconds are dropped: the answer is a whole minute.
        assert_eq!(
            next_match(
                "0 9 * * *",
                moment(2026, 10, 12, 8, 59).with_second(30).unwrap()
            ),
            Some(moment(2026, 10, 12, 9, 0))
        );
    }

    #[test]
    fn next_match_reads_the_weekday_and_gives_up_on_a_hopeless_one() {
        // `monday()` is 2026-10-12; the next Friday 09:30 is the 16th.
        assert_eq!(
            next_match("30 9 * * 5", monday()),
            Some(moment(2026, 10, 16, 9, 30))
        );
        // A malformed expression has no next match, and neither does a date
        // that never exists -- it must not spin until the cap.
        assert_eq!(next_match("not a cron", monday()), None);
        assert_eq!(next_match("0 0 30 2 *", monday()), None);
    }

    #[test]
    fn durable_jobs_survive_a_reload_and_session_jobs_do_not() {
        let (dir, scheduler) = scheduler();
        let durable = scheduler
            .schedule("0 9 * * *", "run tests", true, true)
            .unwrap();
        scheduler
            .schedule("0 10 * * *", "ephemeral", true, false)
            .unwrap();

        let written = std::fs::read_to_string(dir.path().join(SCHEDULED_TASKS_FILE)).unwrap();
        assert!(written.contains("run tests"), "{written}");
        assert!(!written.contains("ephemeral"), "{written}");

        let reloaded = CronScheduler::new(dir.path());
        assert_eq!(reloaded.load().unwrap(), 1);
        let jobs = reloaded.jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, durable.id);
        assert!(jobs[0].durable);
    }

    #[test]
    fn an_empty_prompt_and_a_bad_expression_are_rejected() {
        let (_dir, scheduler) = scheduler();
        assert!(scheduler.schedule("* * * * *", "   ", true, true).is_err());
        assert!(scheduler.schedule("nope", "x", true, true).is_err());
        assert!(scheduler.jobs().is_empty());
    }

    #[test]
    fn poll_marks_a_job_due_once_per_minute() {
        let (_dir, scheduler) = scheduler();
        scheduler
            .schedule("* * * * *", "run tests", true, false)
            .unwrap();

        scheduler.poll(monday()).unwrap();
        scheduler.poll(monday().with_second(30).unwrap()).unwrap();
        assert_eq!(scheduler.pending(), 1);
        assert!(scheduler.has_due());

        let fired = scheduler.take_due();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].prompt, "run tests");
        assert_eq!(scheduler.pending(), 0);

        // Same minute, already fired: the minute marker keeps it quiet.
        scheduler.poll(monday()).unwrap();
        assert_eq!(scheduler.pending(), 0);

        // Acknowledgement re-arms a recurring job, so the next minute fires.
        scheduler.acknowledge(&fired).unwrap();
        scheduler.poll(monday().with_minute(1).unwrap()).unwrap();
        assert_eq!(scheduler.pending(), 1);
    }

    #[test]
    fn a_non_matching_moment_never_queues() {
        let (_dir, scheduler) = scheduler();
        scheduler
            .schedule("0 9 * * *", "morning", true, false)
            .unwrap();
        scheduler.poll(moment(2026, 10, 12, 8, 59)).unwrap();
        assert!(!scheduler.has_due());
    }

    #[test]
    fn recurring_jobs_re_arm_and_one_shot_jobs_vanish() {
        let (_dir, scheduler) = scheduler();
        let recurring = scheduler
            .schedule("*/5 * * * *", "tick", true, false)
            .unwrap();
        let once = scheduler
            .schedule("*/5 * * * *", "once", false, false)
            .unwrap();

        scheduler.poll(monday()).unwrap();
        let delivered = scheduler.take_due();
        assert_eq!(delivered.len(), 2);
        scheduler.acknowledge(&delivered).unwrap();

        assert!(!scheduler.get(&recurring.id).unwrap().pending_delivery);
        assert!(scheduler.get(&once.id).is_none());

        scheduler.poll(monday().with_minute(5).unwrap()).unwrap();
        assert_eq!(scheduler.pending(), 1);
        assert_eq!(scheduler.take_due()[0].id, recurring.id);
    }

    #[test]
    fn restore_puts_a_job_back_after_a_failed_turn() {
        let (_dir, scheduler) = scheduler();
        scheduler
            .schedule("* * * * *", "retry me", true, false)
            .unwrap();
        scheduler.poll(monday()).unwrap();
        let delivered = scheduler.take_due();

        scheduler.restore(&delivered);
        assert!(scheduler.has_due());
        assert_eq!(scheduler.take_due().len(), 1);
    }

    #[test]
    fn cancel_removes_a_job_and_reports_unknown_ids() {
        let (_dir, scheduler) = scheduler();
        let job = scheduler
            .schedule("* * * * *", "adios", true, false)
            .unwrap();
        assert!(scheduler.cancel(&job.id).unwrap().contains(&job.id));
        assert!(scheduler.get(&job.id).is_none());
        assert!(scheduler.cancel("cron_00000000").is_err());
    }

    #[test]
    fn render_lists_every_job() {
        let (_dir, scheduler) = scheduler();
        assert_eq!(scheduler.render(), "No cron jobs.");
        let job = scheduler
            .schedule("0 9 * * 1-5", "weekday standup", true, true)
            .unwrap();
        let rendered = scheduler.render();
        assert!(rendered.contains(&job.id), "{rendered}");
        assert!(rendered.contains("weekday standup"), "{rendered}");
        assert!(rendered.contains("[recurring, durable]"), "{rendered}");
    }

    #[tokio::test]
    async fn the_tools_drive_the_scheduler() {
        let (_dir, scheduler) = scheduler();
        let mut registry = ToolRegistry::new();
        register_cron_tools(&mut registry, scheduler.clone());

        let ctx = ToolContext::new(std::env::temp_dir());
        let call = |name: &str, input: Value| ToolCall {
            id: "call_1".into(),
            name: name.into(),
            input,
        };

        let out = registry
            .execute(
                &ctx,
                &call(
                    SCHEDULE_CRON_TOOL_NAME,
                    json!({ "cron": "*/2 * * * *", "prompt": "check CI" }),
                ),
            )
            .await
            .unwrap();
        assert!(out.contains("Scheduled cron_"), "{out}");

        let listed = registry
            .execute(&ctx, &call(LIST_CRONS_TOOL_NAME, json!({})))
            .await
            .unwrap();
        assert!(listed.contains("check CI"), "{listed}");

        let bad = registry
            .execute(
                &ctx,
                &call(
                    SCHEDULE_CRON_TOOL_NAME,
                    json!({ "cron": "nope", "prompt": "x" }),
                ),
            )
            .await;
        assert!(bad.is_err());

        let job_id = scheduler.jobs()[0].id.clone();
        let cancelled = registry
            .execute(
                &ctx,
                &call(CANCEL_CRON_TOOL_NAME, json!({ "job_id": job_id })),
            )
            .await
            .unwrap();
        assert!(cancelled.starts_with("Cancelled"), "{cancelled}");
        assert!(scheduler.jobs().is_empty());
    }
}
