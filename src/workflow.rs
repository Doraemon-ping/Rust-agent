//! s16: workflow runtime -- the model picks a step, the script picks the path.
//!
//! Up to here every round is model-driven: the model looks at the tool results
//! and decides what to do next. That is right when the next step depends on
//! what the last one found. Some tasks instead repeat a fixed procedure --
//! review four dimensions, verify each finding, merge duplicates, sort by
//! severity -- and the host already knows the steps and their order.
//!
//! The `Workflow` tool exposes that: the model supplies a *name* from the
//! host's registry plus arguments, never code. Behind one `tool_use` a local
//! runtime runs a trusted script that coordinates N subagent calls with four
//! primitives:
//!
//! | Primitive | Meaning |
//! | --- | --- |
//! | `agent(request)` | run one subagent, optionally against a JSON schema |
//! | `parallel(thunks)` | **barrier**: run all of them, wait for all results |
//! | `pipeline(items, stages)` | per-item staged flow, **no barrier** between stages |
//! | `phase(title)` / `log(message)` | progress markers |
//!
//! Every `agent()` result is journalled under a *semantic* key -- a hash of
//! `kind|label|prompt|schema`, never a counter, so concurrent completion order
//! cannot shift the keys. Resuming with `resume_from_run_id` replays the
//! journalled results and only re-runs the calls that changed.
//!
//! Intermediate values live in the script's variables; only the final result
//! and a task summary go back to the conversation. A run keeps its data in
//! `<workspace>/.workflow/`: a snapshot, an output file, and the journal.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::task::JoinSet;

use crate::error::{Error, Result};
use crate::llm::{CompletionRequest, LlmClient};
use crate::message::Message;
use crate::tools::{required_str, Tool, ToolContext, ToolRegistry};

/// The tool the model calls to start (or resume) a saved workflow.
pub const WORKFLOW_TOOL_NAME: &str = "Workflow";

/// Where snapshots, outputs, and journals live, under the workspace.
pub const WORKFLOWS_DIR: &str = ".workflow";

/// The file naming the most recent run, so `resume` can find it.
pub const LAST_RUN_FILE: &str = "last_run.txt";

/// System-prompt nudge describing the tool.
pub const WORKFLOW_PROMPT: &str =
    "Saved multi-step procedures are exposed as the Workflow tool: pass a workflow \
     name and its args to run a scripted, resumable orchestration instead of driving \
     each step yourself. Pass resume_from_run_id to continue an interrupted run.";

/// Hard cap on `agent()` calls in one run, nested workflows included.
pub const AGENT_CAP: usize = 1000;

/// How many `agent()` calls may be in flight at once.
pub const CONCURRENCY: usize = 8;

/// Token budget for one workflow agent call.
pub const WORKFLOW_AGENT_MAX_TOKENS: u32 = 2_000;

/// The system prompt every workflow subagent runs under.
pub const WORKFLOW_AGENT_SYSTEM: &str =
    "You are a focused workflow agent. Complete only the supplied step. \
     Do not claim access to files or results not included in the prompt.";

/// A boxed, owned workflow future.
pub type BoxWorkflowFuture = Pin<Box<dyn Future<Output = Result<Value>> + Send + 'static>>;

/// A saved procedure: takes the run context and the run arguments.
pub type WorkflowScript = Arc<dyn Fn(ExecutionContext, Value) -> BoxWorkflowFuture + Send + Sync>;

/// One stage of a [`ExecutionContext::pipeline`], given the previous value,
/// the original item, and the item's index.
pub type PipelineStage =
    Arc<dyn Fn(ExecutionContext, Value, Value, usize) -> BoxWorkflowFuture + Send + Sync>;

/// A run-wide event sink, wired by the host to its observer.
pub type WorkflowListener = Arc<dyn Fn(&WorkflowEvent) + Send + Sync>;

// -- Stable helpers ------------------------------------------------------

/// Process-stable 64-bit FNV-1a hash.
///
/// Resume keys have to survive a process restart, so `DefaultHasher` (whose
/// seed is chosen per process) is out. FNV-1a is small, deterministic, and
/// more than good enough to name a journal record.
pub fn stable_hash(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Deterministic JSON with sorted object keys, so a re-serialised schema
/// hashes the same on resume.
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|key| {
                    let encoded = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string());
                    let inner = map.get(key.as_str()).cloned().unwrap_or(Value::Null);
                    format!("{encoded}:{}", canonical_json(&inner))
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
    }
}

/// The first `max` characters of `text`.
fn take_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// A 16-hex-character token, unique enough to name a run before the exclusive
/// snapshot create confirms it.
fn random_hex16() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = u64::from(std::process::id());
    let basis = format!("{nanos}-{counter}-{pid}");
    let first = stable_hash(&basis);
    let second = stable_hash(&format!("{basis}#{first}"));
    format!("{first:016x}{second:016x}")
        .chars()
        .take(16)
        .collect()
}

/// Whether `name` is a legal workflow slug: 1-64 characters from
/// `[A-Za-z0-9._-]`, starting with an alphanumeric.
pub fn valid_workflow_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    if !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'.' || *byte == b'_' || *byte == b'-')
}

/// Whether `run_id` matches `wf_<name>_<16 lowercase hex>`.
pub fn valid_run_id(run_id: &str) -> bool {
    let rest = match run_id.strip_prefix("wf_") {
        Some(rest) => rest,
        None => return false,
    };
    if rest.len() < 18 {
        return false;
    }
    let (name, tail) = rest.split_at(rest.len() - 17);
    let hex = match tail.strip_prefix('_') {
        Some(hex) => hex,
        None => return false,
    };
    valid_workflow_name(name)
        && hex.len() == 16
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// -- Metadata ------------------------------------------------------------

/// A saved workflow's declared identity: what it is called, what it does, and
/// the phase labels it reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowMeta {
    /// Slug the model passes to the tool.
    pub name: String,
    /// One-line description of the procedure.
    pub description: String,
    /// Optional ordered phase labels for progress reporting.
    pub phases: Vec<String>,
}

impl WorkflowMeta {
    /// Build metadata, enforcing the name/description/phases contract.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        phases: Vec<String>,
    ) -> Result<Self> {
        let meta = WorkflowMeta {
            name: name.into(),
            description: description.into(),
            phases,
        };
        meta.validate()?;
        Ok(meta)
    }

    /// Validate the record before launch (`validate_meta` in the reference).
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(Error::Workflow("meta requires a `name`".to_string()));
        }
        if !valid_workflow_name(&self.name) {
            return Err(Error::Workflow(
                "meta.name must be a 1-64 character slug using letters, numbers, '.', '_', or '-'"
                    .to_string(),
            ));
        }
        if self.description.trim().is_empty() {
            return Err(Error::Workflow("meta requires a `description`".to_string()));
        }
        if self.phases.iter().any(|phase| phase.trim().is_empty()) {
            return Err(Error::Workflow(
                "meta.phases must be a list of non-empty strings".to_string(),
            ));
        }
        Ok(())
    }

    /// The `phases` list rendered for a one-line log (`-` when empty).
    pub fn phases_label(&self) -> String {
        if self.phases.is_empty() {
            "-".to_string()
        } else {
            self.phases.join(",")
        }
    }
}

// -- Minimal JSON Schema -------------------------------------------------

/// Validate `value` against a tiny JSON-schema subset: objects (with
/// `required`/`properties`), arrays (with `items`), strings, booleans,
/// numbers/integers, and `enum`.
pub fn validate_schema(schema: &Value, value: &Value) -> std::result::Result<(), String> {
    if let Some(Value::Array(options)) = schema.get("enum") {
        if !options.iter().any(|option| option == value) {
            let rendered = serde_json::to_string(options).unwrap_or_default();
            return Err(format!("expected one of {rendered}"));
        }
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let object = value.as_object().ok_or("expected object")?;
            if let Some(Value::Array(required)) = schema.get("required") {
                for key in required.iter().filter_map(Value::as_str) {
                    if !object.contains_key(key) {
                        return Err(format!("missing required key '{key}'"));
                    }
                }
            }
            if let Some(Value::Object(properties)) = schema.get("properties") {
                for (key, sub) in properties {
                    if let Some(inner) = object.get(key) {
                        validate_schema(sub, inner).map_err(|error| format!("{key}: {error}"))?;
                    }
                }
            }
            Ok(())
        }
        Some("array") => {
            let items = value.as_array().ok_or("expected array")?;
            if let Some(item_schema) = schema.get("items") {
                for (index, element) in items.iter().enumerate() {
                    validate_schema(item_schema, element)
                        .map_err(|error| format!("[{index}]: {error}"))?;
                }
            }
            Ok(())
        }
        Some("string") => match value {
            Value::String(_) => Ok(()),
            _ => Err("expected string".to_string()),
        },
        Some("boolean") => match value {
            Value::Bool(_) => Ok(()),
            _ => Err("expected boolean".to_string()),
        },
        Some("number") | Some("integer") => match value {
            Value::Number(_) => Ok(()),
            _ => Err("expected number".to_string()),
        },
        _ => Ok(()),
    }
}

/// Deterministic generic filler for schemas the mock runner does not
/// special-case (`_fill_schema` in the reference).
pub fn fill_schema(schema: &Value, seed: &str) -> Value {
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let keys: Vec<String> = match schema.get("required") {
                Some(Value::Array(required)) => required
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                _ => schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|properties| properties.keys().cloned().collect())
                    .unwrap_or_default(),
            };
            let mut object = serde_json::Map::new();
            for key in keys {
                let sub = schema
                    .get("properties")
                    .and_then(|properties| properties.get(key.as_str()))
                    .cloned()
                    .unwrap_or(Value::Null);
                let filled = fill_schema(&sub, &format!("{seed}/{key}"));
                object.insert(key, filled);
            }
            Value::Object(object)
        }
        Some("array") => {
            let item = schema.get("items").cloned().unwrap_or(Value::Null);
            Value::Array(vec![fill_schema(&item, &format!("{seed}/0"))])
        }
        Some("boolean") => Value::Bool(stable_hash(seed) % 4 != 0),
        Some("number") | Some("integer") => {
            Value::Number(serde_json::Number::from(stable_hash(seed) % 5))
        }
        _ => Value::String(seed.rsplit('/').next().unwrap_or(seed).to_string()),
    }
}

// -- Agent runners -------------------------------------------------------

/// One workflow agent's output.
#[derive(Debug, Clone, PartialEq)]
pub struct RunnerOutput {
    /// The value the step produced.
    pub value: Value,
    /// Tokens the step billed (best effort).
    pub tokens: u64,
}

/// The seam between the workflow runtime and whatever actually answers an
/// `agent()` step.
#[async_trait]
pub trait AgentRunner: Send + Sync {
    /// Answer one step, optionally constrained to `schema`.
    async fn run(
        &self,
        prompt: &str,
        schema: Option<&Value>,
        label: Option<&str>,
    ) -> Result<RunnerOutput>;
}

/// Deterministic runner used by the demo and by unit tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct MockAgentRunner;

/// A rough token estimate for mock outputs.
fn token_estimate(prompt: &str, value: &Value) -> u64 {
    let rendered = serde_json::to_string(value).unwrap_or_default();
    (prompt.chars().count() / 4 + rendered.chars().count() / 4) as u64
}

#[async_trait]
impl AgentRunner for MockAgentRunner {
    async fn run(
        &self,
        prompt: &str,
        schema: Option<&Value>,
        label: Option<&str>,
    ) -> Result<RunnerOutput> {
        let value = match schema {
            None => {
                let source = label.unwrap_or(prompt);
                Value::String(format!("[mock] {}", take_chars(source, 60)))
            }
            Some(schema) => {
                let properties = schema.get("properties").and_then(Value::as_object);
                let has = |key: &str| {
                    properties
                        .map(|props| props.contains_key(key))
                        .unwrap_or(false)
                };
                if has("findings") {
                    let count = 1 + (stable_hash(prompt) % 2) as usize;
                    let severities = ["high", "medium", "low"];
                    let findings: Vec<Value> = (0..count)
                        .map(|index| {
                            let severity =
                                severities[(stable_hash(&format!("{prompt}{index}")) % 3) as usize];
                            json!({
                                "title": format!("{} #{}", label.unwrap_or("audit"), index + 1),
                                "severity": severity,
                            })
                        })
                        .collect();
                    json!({ "findings": findings })
                } else if has("isReal") {
                    let real = stable_hash(prompt) % 4 != 0;
                    json!({
                        "isReal": real,
                        "reason": if real { "reproduced" } else { "could not reproduce" },
                    })
                } else {
                    fill_schema(schema, prompt)
                }
            }
        };
        let tokens = token_estimate(prompt, &value);
        Ok(RunnerOutput { value, tokens })
    }
}

/// The environment note prepended to every workflow agent's system prompt.
fn workflow_agent_environment() -> &'static str {
    if cfg!(windows) {
        "Host OS: Windows. This workflow agent only responds to its supplied step; it has no direct shell or file tools."
    } else {
        "Host OS: Unix-like. This workflow agent only responds to its supplied step; it has no direct shell or file tools."
    }
}

/// Parse a model answer into JSON, tolerating a fenced code block or a JSON
/// object embedded in surrounding prose.
fn parse_runner_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    let stripped = if let Some(rest) = trimmed.strip_prefix("```") {
        let body = match rest.find('\n') {
            Some(index) => &rest[index + 1..],
            None => "",
        };
        let body = body.trim_end();
        body.strip_suffix("```").map(str::trim_end).unwrap_or(body)
    } else {
        trimmed
    };
    if let Ok(value) = serde_json::from_str::<Value>(stripped) {
        return Some(value);
    }
    for (index, _) in stripped.char_indices().filter(|(_, ch)| *ch == '{') {
        let mut deserializer = serde_json::Deserializer::from_str(&stripped[index..]);
        if let Ok(value) = Value::deserialize(&mut deserializer) {
            return Some(value);
        }
    }
    None
}

/// Run workflow agents through the host's own model client.
pub struct AnthropicAgentRunner {
    client: Arc<dyn LlmClient>,
    model: Option<String>,
    max_tokens: u32,
}

impl AnthropicAgentRunner {
    /// Wrap a shared client, defaulting to its configured model.
    pub fn new(client: Arc<dyn LlmClient>) -> Self {
        Self {
            client,
            model: None,
            max_tokens: WORKFLOW_AGENT_MAX_TOKENS,
        }
    }

    /// Pin a specific model for workflow steps.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Override the per-step token budget.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

#[async_trait]
impl AgentRunner for AnthropicAgentRunner {
    async fn run(
        &self,
        prompt: &str,
        schema: Option<&Value>,
        _label: Option<&str>,
    ) -> Result<RunnerOutput> {
        let mut request = prompt.to_string();
        if let Some(schema) = schema {
            request.push_str("\n\nReturn only one JSON object matching this schema:\n");
            request.push_str(&canonical_json(schema));
        }
        let message = Message::user_text(request);
        let system = format!("{WORKFLOW_AGENT_SYSTEM} {}", workflow_agent_environment());
        let completion = self
            .client
            .complete(CompletionRequest {
                model: self.model.as_deref(),
                system: &system,
                messages: std::slice::from_ref(&message),
                tools: &[],
                max_tokens: self.max_tokens,
            })
            .await?;
        let text = completion.text();
        let value = match schema {
            None => Value::String(text.trim().to_string()),
            Some(_) => {
                parse_runner_json(&text).unwrap_or_else(|| Value::String(text.trim().to_string()))
            }
        };
        let tokens = completion
            .usage
            .map(|usage| u64::from(usage.input_tokens) + u64::from(usage.output_tokens))
            .unwrap_or(0);
        Ok(RunnerOutput { value, tokens })
    }
}
// -- Journal -------------------------------------------------------------

/// Append-only `<runId>.journal.jsonl`.
///
/// On resume, an `agent()` call whose semantic key is already recorded is
/// replayed from the cache instead of being re-run.
pub struct WorkflowJournal {
    path: PathBuf,
    file: Mutex<File>,
    cache: Mutex<BTreeMap<String, Value>>,
}

impl WorkflowJournal {
    /// Open the journal for `run_id` under `store`.
    ///
    /// A fresh run truncates any stale file; a resume requires the file to
    /// exist and replays it into the cache.
    pub fn open(store: &Path, run_id: &str, resume: bool) -> Result<Self> {
        std::fs::create_dir_all(store)?;
        let path = store.join(format!("{run_id}.journal.jsonl"));
        if resume {
            if !path.exists() {
                return Err(Error::Workflow(format!(
                    "resume journal not found for {run_id}"
                )));
            }
            let text = std::fs::read_to_string(&path)?;
            let mut cache = BTreeMap::new();
            for (index, line) in text.lines().enumerate() {
                let record = line.trim();
                if record.is_empty() {
                    continue;
                }
                let invalid = || {
                    Error::Workflow(format!(
                        "invalid resume journal record at line {}",
                        index + 1
                    ))
                };
                let parsed: Value = serde_json::from_str(record).map_err(|_| invalid())?;
                let key = parsed
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?;
                let value = parsed.get("value").cloned().ok_or_else(invalid)?;
                cache.insert(key.to_string(), value);
            }
            let file = OpenOptions::new().append(true).open(&path)?;
            Ok(Self {
                path,
                file: Mutex::new(file),
                cache: Mutex::new(cache),
            })
        } else {
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)?;
            Ok(Self {
                path,
                file: Mutex::new(file),
                cache: Mutex::new(BTreeMap::new()),
            })
        }
    }

    /// The journal path on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The semantic key for one `agent()` call: a hash of
    /// `kind|label|prompt|schema`, deliberately independent of call order so a
    /// resumed parallel/pipeline run maps onto the same records.
    pub fn key(kind: &str, label: &str, prompt: &str, schema: Option<&Value>) -> String {
        let schema_json = schema
            .map(canonical_json)
            .unwrap_or_else(|| "null".to_string());
        let basis = format!("{kind}|{label}|{prompt}|{schema_json}");
        format!("{kind}-{:010}", stable_hash(&basis) % 10_000_000_000)
    }

    /// A previously recorded value, if any.
    pub fn cached(&self, key: &str) -> Option<Value> {
        self.cache.lock().unwrap().get(key).cloned()
    }

    /// Append a record and remember it in the cache.
    pub fn record(&self, key: &str, value: &Value) -> Result<()> {
        let line = serde_json::to_string(&json!({ "key": key, "value": value }))?;
        {
            let mut file = self.file.lock().unwrap();
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
            file.flush()?;
        }
        self.cache
            .lock()
            .unwrap()
            .insert(key.to_string(), value.clone());
        Ok(())
    }
}

// -- Token budget --------------------------------------------------------

/// Token budget shared by a whole run, nested workflows included. Once the
/// spend would pass `total`, `agent()` refuses instead of silently
/// overspending.
#[derive(Debug)]
pub struct Budget {
    total: Option<u64>,
    spent: Mutex<u64>,
}

impl Budget {
    /// A budget capped at `total` tokens (`None` is unbounded).
    pub fn new(total: Option<u64>) -> Self {
        Self {
            total,
            spent: Mutex::new(0),
        }
    }

    /// Charge `tokens`, refusing to let the total be exceeded.
    pub fn add(&self, tokens: u64) -> Result<()> {
        let mut spent = self.spent.lock().unwrap();
        let next = spent.saturating_add(tokens);
        if let Some(total) = self.total {
            if next > total {
                return Err(Error::Workflow(format!(
                    "token budget exceeded ({next} > {total})"
                )));
            }
        }
        *spent = next;
        Ok(())
    }

    /// Tokens charged so far.
    pub fn spent(&self) -> u64 {
        *self.spent.lock().unwrap()
    }

    /// Tokens left, or `None` when the budget is unbounded.
    pub fn remaining(&self) -> Option<u64> {
        self.total.map(|total| total.saturating_sub(self.spent()))
    }
}

// -- Task state ----------------------------------------------------------

/// One progress line a workflow emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressEvent {
    /// `workflow_phase`, `workflow_log`, or `workflow_agent`.
    pub kind: String,
    /// The event's fields, in stable key order.
    pub data: BTreeMap<String, String>,
}

/// Live state of one workflow run: status, usage counters, and the progress
/// trail. Shared behind a mutex so concurrent `agent()` calls can append.
#[derive(Debug)]
pub struct WorkflowTask {
    /// Identifier the model sees (`local_workflow_<runId>`).
    pub task_id: String,
    /// The run this task tracks.
    pub run_id: String,
    /// Name of the workflow being run.
    pub name: String,
    /// `running`, `completed`, or `failed`.
    pub status: String,
    /// `agent()` calls recorded.
    pub agents: u64,
    /// Tokens recorded.
    pub tokens: u64,
    /// Progress events in emission order.
    pub progress: Vec<ProgressEvent>,
}

impl WorkflowTask {
    fn new(task_id: String, run_id: String, name: String) -> Self {
        Self {
            task_id,
            run_id,
            name,
            status: "running".to_string(),
            agents: 0,
            tokens: 0,
            progress: Vec::new(),
        }
    }

    /// The JSON summary handed back to the model.
    pub fn serialize(&self) -> Value {
        let progress: Vec<Value> = self
            .progress
            .iter()
            .map(|event| {
                let mut object = serde_json::Map::new();
                object.insert("type".to_string(), Value::String(event.kind.clone()));
                for (key, value) in &event.data {
                    object.insert(key.clone(), Value::String(value.clone()));
                }
                Value::Object(object)
            })
            .collect();
        json!({
            "taskId": self.task_id,
            "taskType": "local_workflow",
            "runId": self.run_id,
            "workflowName": self.name,
            "status": self.status,
            "usage": { "agents": self.agents, "tokens": self.tokens },
            "progress": progress,
        })
    }
}

// -- Run-wide limits -----------------------------------------------------

/// Guards shared by every nested workflow of one run: the hard `agent()` cap
/// and the concurrency semaphore.
pub struct ExecutionLimits {
    agents: AtomicUsize,
    semaphore: Semaphore,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutionLimits {
    /// Fresh limits: no agents claimed, `CONCURRENCY` slots free.
    pub fn new() -> Self {
        Self {
            agents: AtomicUsize::new(0),
            semaphore: Semaphore::new(CONCURRENCY),
        }
    }

    /// Count an `agent()` call, refusing past `AGENT_CAP`.
    pub fn claim_agent(&self) -> Result<()> {
        let claimed = self.agents.fetch_add(1, Ordering::SeqCst) + 1;
        if claimed > AGENT_CAP {
            return Err(Error::Workflow(format!(
                "agent() cap reached ({AGENT_CAP})"
            )));
        }
        Ok(())
    }

    /// Agents claimed so far.
    pub fn claimed(&self) -> usize {
        self.agents.load(Ordering::SeqCst)
    }

    async fn acquire(&self) -> SemaphorePermit<'_> {
        self.semaphore
            .acquire()
            .await
            .expect("workflow semaphore is never closed")
    }
}

// -- Events --------------------------------------------------------------

/// A lifecycle or progress line a run reports to its listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowEvent {
    /// The run reporting the event.
    pub run_id: String,
    /// `async_launched`, `task_started`, `workflow_phase`, `workflow_log`,
    /// `workflow_agent`, or `task_notification`.
    pub kind: String,
    /// A rendered one-line description.
    pub message: String,
}

// -- Execution context ---------------------------------------------------

/// The shared body of a live run. [`ExecutionContext`] is a cheap clone of an
/// `Arc` to it, so a script can hand clones to stages and futures.
struct ContextInner {
    task: Arc<Mutex<WorkflowTask>>,
    journal: Arc<WorkflowJournal>,
    runner: Arc<dyn AgentRunner>,
    budget: Arc<Budget>,
    limits: Arc<ExecutionLimits>,
    registry: WorkflowRegistry,
    args: Value,
    depth: usize,
    phase: Mutex<Option<String>>,
    phases_seen: Mutex<BTreeSet<String>>,
    listener: Option<WorkflowListener>,
}

/// The context injected into a workflow script: the orchestration primitives
/// (`agent`, `parallel`, `pipeline`, `phase`, `log`) plus the run's journal,
/// budget, task, and shared limits.
///
/// Cloning is cheap (an `Arc` bump); the futures returned by [`Self::agent`]
/// own their context, so a script can build a batch and await it later.
#[derive(Clone)]
pub struct ExecutionContext {
    inner: Arc<ContextInner>,
}

impl ExecutionContext {
    /// The arguments the run was launched with.
    pub fn args(&self) -> &Value {
        &self.inner.args
    }

    /// Start a phase. Upsert: re-emitting a phase (say, from every pipeline
    /// item) announces it only once.
    pub fn phase(&self, title: &str) {
        *self.inner.phase.lock().unwrap() = Some(title.to_string());
        let first = self
            .inner
            .phases_seen
            .lock()
            .unwrap()
            .insert(title.to_string());
        if first {
            let mut data = BTreeMap::new();
            data.insert("title".to_string(), title.to_string());
            self.progress("workflow_phase", data, format!("phase {title}"));
        }
    }

    /// Emit a `workflow_log` progress line.
    pub fn log(&self, message: &str) {
        let mut data = BTreeMap::new();
        data.insert("message".to_string(), message.to_string());
        self.progress("workflow_log", data, message.to_string());
    }

    /// Run one subagent step.
    ///
    /// Returns an awaitable rather than awaiting inline, so callers can collect
    /// a batch with [`Self::parallel`] or thread it through a [`PipelineStage`].
    /// With a `schema`, the output is validated and re-asked once on a miss.
    pub fn agent(
        &self,
        prompt: impl Into<String>,
        schema: Option<Value>,
        label: Option<String>,
        phase: Option<String>,
    ) -> BoxWorkflowFuture {
        let ctx = self.clone();
        let prompt = prompt.into();
        Box::pin(async move { ctx.run_agent(prompt, schema, label, phase).await })
    }

    /// The body of one `agent()` call: journal lookup, the concurrency
    /// semaphore, schema validation with one retry, and budget accounting.
    async fn run_agent(
        &self,
        prompt: String,
        schema: Option<Value>,
        label: Option<String>,
        phase: Option<String>,
    ) -> Result<Value> {
        let label = label.unwrap_or_else(|| format!("{}...", take_chars(&prompt, 24)));
        let phase = phase.or_else(|| self.inner.phase.lock().unwrap().clone());
        self.inner.limits.claim_agent()?;
        if self.inner.budget.remaining() == Some(0) {
            return Err(Error::Workflow("token budget exceeded".to_string()));
        }

        let key = WorkflowJournal::key("agent", &label, &prompt, schema.as_ref());
        if let Some(cached) = self.inner.journal.cached(&key) {
            if let Some(schema) = &schema {
                if let Err(error) = validate_schema(schema, &cached) {
                    return Err(Error::Workflow(format!(
                        "cached agent output failed schema validation: {error}"
                    )));
                }
            }
            self.agent_progress(&label, phase.as_deref(), "cached");
            return Ok(cached);
        }

        let permit = self.inner.limits.acquire().await;
        let first = self
            .inner
            .runner
            .run(&prompt, schema.as_ref(), Some(&label))
            .await?;
        drop(permit);
        let mut value = first.value;
        let mut tokens = first.tokens;

        if let Some(schema) = &schema {
            if validate_schema(schema, &value).is_err() {
                let retry = self
                    .inner
                    .runner
                    .run(
                        &format!("{prompt}\n\nReturn valid JSON."),
                        Some(schema),
                        Some(&label),
                    )
                    .await?;
                tokens += retry.tokens;
                value = retry.value;
                if let Err(error) = validate_schema(schema, &value) {
                    return Err(Error::Workflow(format!(
                        "agent({{schema}}) invalid output: {error}"
                    )));
                }
            }
        }

        self.inner.budget.add(tokens)?;
        {
            let mut task = self.inner.task.lock().unwrap();
            task.agents += 1;
            task.tokens += tokens;
        }
        self.inner.journal.record(&key, &value)?;
        self.agent_progress(&label, phase.as_deref(), "done");
        Ok(value)
    }

    /// Record one `workflow_agent` progress line.
    fn agent_progress(&self, label: &str, phase: Option<&str>, status: &str) {
        let mut data = BTreeMap::new();
        data.insert("label".to_string(), label.to_string());
        data.insert("phase".to_string(), phase.unwrap_or("-").to_string());
        data.insert("status".to_string(), status.to_string());
        self.progress("workflow_agent", data, format!("agent {label} ({status})"));
    }

    /// Append a progress line to the task and forward it to the host listener.
    fn progress(&self, kind: &str, data: BTreeMap<String, String>, message: String) {
        let run_id = {
            let mut task = self.inner.task.lock().unwrap();
            task.progress.push(ProgressEvent {
                kind: kind.to_string(),
                data,
            });
            task.run_id.clone()
        };
        if let Some(listener) = &self.inner.listener {
            listener(&WorkflowEvent {
                run_id,
                kind: kind.to_string(),
                message,
            });
        }
    }

    /// **Barrier**: run every thunk concurrently and return their results in
    /// input order, failing if any thunk fails.
    pub async fn parallel(&self, thunks: Vec<BoxWorkflowFuture>) -> Result<Vec<Value>> {
        let count = thunks.len();
        let mut set = JoinSet::new();
        for (index, thunk) in thunks.into_iter().enumerate() {
            set.spawn(async move { (index, thunk.await) });
        }
        let mut slots: Vec<Option<Value>> = (0..count).map(|_| None).collect();
        let mut failure: Option<Error> = None;
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((index, Ok(value))) => slots[index] = Some(value),
                Ok((_, Err(error))) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(Error::Workflow(format!("workflow step panicked: {error}")));
                    }
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(slots
                .into_iter()
                .map(|slot| slot.unwrap_or(Value::Null))
                .collect()),
        }
    }

    /// Per-item staged flow with **no barrier** between stages: item A can be
    /// in stage 3 while item B is still in stage 1. Each stage receives the
    /// previous value, the original item, and the item's index.
    pub async fn pipeline(
        &self,
        items: Vec<Value>,
        stages: Vec<PipelineStage>,
    ) -> Result<Vec<Value>> {
        let mut thunks: Vec<BoxWorkflowFuture> = Vec::with_capacity(items.len());
        for (index, item) in items.into_iter().enumerate() {
            let ctx = self.clone();
            let stages = stages.clone();
            thunks.push(Box::pin(async move {
                run_pipeline_item(ctx, item, stages, index).await
            }));
        }
        self.parallel(thunks).await
    }

    /// Run a saved workflow inline as a child, sharing this run's journal,
    /// budget, and agent counter. Nesting is one level only.
    pub async fn workflow(&self, name: &str, args: Value) -> Result<Value> {
        if self.inner.depth >= 1 {
            return Err(Error::Workflow(
                "workflow() nesting is one level only".to_string(),
            ));
        }
        let script = self
            .inner
            .registry
            .script(name)
            .ok_or_else(|| Error::Workflow(format!("unknown workflow '{name}'")))?;
        let child = ExecutionContext {
            inner: Arc::new(ContextInner {
                task: Arc::clone(&self.inner.task),
                journal: Arc::clone(&self.inner.journal),
                runner: Arc::clone(&self.inner.runner),
                budget: Arc::clone(&self.inner.budget),
                limits: Arc::clone(&self.inner.limits),
                registry: self.inner.registry.clone(),
                args: args.clone(),
                depth: self.inner.depth + 1,
                phase: Mutex::new(None),
                phases_seen: Mutex::new(BTreeSet::new()),
                listener: self.inner.listener.clone(),
            }),
        };
        script(child, args).await
    }
}

/// Run one item through every stage in order.
async fn run_pipeline_item(
    ctx: ExecutionContext,
    item: Value,
    stages: Vec<PipelineStage>,
    index: usize,
) -> Result<Value> {
    let mut value = item.clone();
    for stage in &stages {
        value = stage(ctx.clone(), value, item.clone(), index).await?;
    }
    Ok(value)
}

// -- Registry ------------------------------------------------------------

/// One saved procedure plus its declared identity.
struct RegisteredWorkflow {
    meta: WorkflowMeta,
    script: WorkflowScript,
}

struct RegistryInner {
    workflows: Mutex<BTreeMap<String, RegisteredWorkflow>>,
    listener: Mutex<Option<WorkflowListener>>,
    active_runs: Mutex<BTreeSet<String>>,
    runner: Mutex<Arc<dyn AgentRunner>>,
}

impl Default for RegistryInner {
    fn default() -> Self {
        RegistryInner {
            workflows: Mutex::new(BTreeMap::new()),
            listener: Mutex::new(None),
            active_runs: Mutex::new(BTreeSet::new()),
            runner: Mutex::new(Arc::new(MockAgentRunner)),
        }
    }
}

/// The host's trusted set of saved workflows, keyed by name.
///
/// The model only ever passes a name; the script itself is host code, so a
/// workflow can never smuggle new instructions into the loop. Cheap to clone
/// (shared state), like the cron scheduler and the team runtime.
#[derive(Clone, Default)]
pub struct WorkflowRegistry {
    inner: Arc<RegistryInner>,
}

impl WorkflowRegistry {
    /// An empty registry whose steps run through the deterministic mock.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the sink lifecycle and progress events are reported to.
    pub fn with_listener(self, listener: WorkflowListener) -> Self {
        self.set_listener(listener);
        self
    }

    /// Replace the event sink.
    pub fn set_listener(&self, listener: WorkflowListener) {
        *self.inner.listener.lock().unwrap() = Some(listener);
    }

    /// Set the runner that answers workflow steps.
    pub fn with_runner(self, runner: Arc<dyn AgentRunner>) -> Self {
        self.set_runner(runner);
        self
    }

    /// Replace the runner that answers workflow steps.
    pub fn set_runner(&self, runner: Arc<dyn AgentRunner>) {
        *self.inner.runner.lock().unwrap() = runner;
    }

    /// Register a saved procedure under its declared name.
    pub fn register(&self, meta: WorkflowMeta, script: WorkflowScript) -> Result<()> {
        meta.validate()?;
        self.inner
            .workflows
            .lock()
            .unwrap()
            .insert(meta.name.clone(), RegisteredWorkflow { meta, script });
        Ok(())
    }

    /// Every registered workflow name, in order.
    pub fn names(&self) -> Vec<String> {
        self.inner
            .workflows
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    /// Whether `name` is registered.
    pub fn has(&self, name: &str) -> bool {
        self.inner.workflows.lock().unwrap().contains_key(name)
    }

    /// The script registered under `name`.
    pub fn script(&self, name: &str) -> Option<WorkflowScript> {
        self.inner
            .workflows
            .lock()
            .unwrap()
            .get(name)
            .map(|registered| Arc::clone(&registered.script))
    }

    /// A one-line catalog of the registered workflows.
    pub fn catalog(&self) -> String {
        let workflows = self.inner.workflows.lock().unwrap();
        if workflows.is_empty() {
            return "(none registered)".to_string();
        }
        workflows
            .values()
            .map(|registered| format!("{}: {}", registered.meta.name, registered.meta.description))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// The declared identity of `name`, if registered.
    pub fn meta(&self, name: &str) -> Option<WorkflowMeta> {
        self.inner
            .workflows
            .lock()
            .unwrap()
            .get(name)
            .map(|registered| registered.meta.clone())
    }

    /// Run (or resume) `name` against `workspace`, returning the launch
    /// envelope the tool hands back to the model.
    pub async fn run(
        &self,
        workspace: &Path,
        name: &str,
        args: Option<Value>,
        resume_from: Option<String>,
    ) -> Result<Value> {
        let meta = self
            .meta(name)
            .ok_or_else(|| Error::Workflow(format!("unknown workflow '{name}'")))?;
        let script = self
            .script(name)
            .ok_or_else(|| Error::Workflow(format!("unknown workflow '{name}'")))?;
        if let Some(args) = &args {
            if !args.is_object() {
                return Err(Error::Workflow(
                    "workflow args must be an object".to_string(),
                ));
            }
        }

        let store = workspace.join(WORKFLOWS_DIR);
        std::fs::create_dir_all(&store)?;

        let resuming = resume_from.is_some();
        let run_id = match resume_from {
            Some(run_id) => {
                if !valid_run_id(&run_id) {
                    return Err(Error::Workflow("invalid workflow runId".to_string()));
                }
                run_id
            }
            None => reserve_run_id(&store, &meta.name)?,
        };

        {
            let mut active = self.inner.active_runs.lock().unwrap();
            if !active.insert(run_id.clone()) {
                return Err(Error::Workflow(format!(
                    "workflow run {run_id} is already active"
                )));
            }
        }
        let outcome = self
            .run_locked(&store, &meta, &script, args, run_id.clone(), resuming)
            .await;
        self.inner.active_runs.lock().unwrap().remove(&run_id);
        outcome
    }

    /// The part of a run that happens under the run's exclusive lock: resume
    /// matching, the journal, the lifecycle envelope, and the script itself.
    async fn run_locked(
        &self,
        store: &Path,
        meta: &WorkflowMeta,
        script: &WorkflowScript,
        args: Option<Value>,
        run_id: String,
        resuming: bool,
    ) -> Result<Value> {
        let args = if resuming {
            let snapshot = read_snapshot(store, &run_id)?;
            let saved_name = snapshot
                .get("workflowName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if saved_name != meta.name {
                return Err(Error::Workflow(
                    "resume runId does not match workflow meta".to_string(),
                ));
            }
            let saved_args = snapshot.get("args").cloned().unwrap_or_else(|| json!({}));
            match args {
                None => saved_args,
                Some(args) if args == saved_args => args,
                Some(_) => {
                    return Err(Error::Workflow(
                        "resume args do not match the original run".to_string(),
                    ))
                }
            }
        } else {
            args.unwrap_or_else(|| json!({}))
        };

        let journal = Arc::new(WorkflowJournal::open(store, &run_id, resuming)?);
        let task = Arc::new(Mutex::new(WorkflowTask::new(
            create_task_id(&run_id),
            run_id.clone(),
            meta.name.clone(),
        )));
        let listener = self.inner.listener.lock().unwrap().clone();
        emit_event(
            &listener,
            &run_id,
            "async_launched",
            format!("launched {run_id}"),
        );
        emit_event(
            &listener,
            &run_id,
            "task_started",
            format!("{} phases={}", meta.name, meta.phases_label()),
        );
        write_snapshot(store, &run_id, &meta.name, &args, &task)?;

        let runner = Arc::clone(&self.inner.runner.lock().unwrap());
        let context = ExecutionContext {
            inner: Arc::new(ContextInner {
                task: Arc::clone(&task),
                journal: Arc::clone(&journal),
                runner,
                budget: Arc::new(Budget::new(args.get("budget").and_then(Value::as_u64))),
                limits: Arc::new(ExecutionLimits::new()),
                registry: self.clone(),
                args: args.clone(),
                depth: 0,
                phase: Mutex::new(None),
                phases_seen: Mutex::new(BTreeSet::new()),
                listener: listener.clone(),
            }),
        };

        let result = match script(context, args.clone()).await {
            Ok(value) => {
                task.lock().unwrap().status = "completed".to_string();
                value
            }
            Err(error) => {
                task.lock().unwrap().status = "failed".to_string();
                json!({ "error": error.to_string() })
            }
        };

        write_json(&store.join(format!("{run_id}.output.json")), &result)?;
        write_snapshot(store, &run_id, &meta.name, &args, &task)?;
        save_last_run(store, &run_id)?;

        let (status, agents, tokens) = {
            let task = task.lock().unwrap();
            (task.status.clone(), task.agents, task.tokens)
        };
        emit_event(
            &listener,
            &run_id,
            "task_notification",
            format!(
                "status={status} agents={agents} tokens={tokens} \
                 outputFile={WORKFLOWS_DIR}/{run_id}.output.json"
            ),
        );

        let task_json = task.lock().unwrap().serialize();
        Ok(json!({
            "launched": {
                "status": "async_launched",
                "taskId": create_task_id(&run_id),
                "taskType": "local_workflow",
                "runId": run_id,
                "workflowName": meta.name,
            },
            "result": result,
            "task": task_json,
        }))
    }
}

// -- Run files -----------------------------------------------------------

/// Reserve a fresh run identity before any journal can be truncated: the
/// exclusive create of the snapshot *is* the reservation.
fn reserve_run_id(store: &Path, name: &str) -> Result<String> {
    for _ in 0..32 {
        let run_id = format!("wf_{name}_{}", random_hex16());
        if !valid_run_id(&run_id) {
            continue;
        }
        let path = store.join(format!("{run_id}.json"));
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(_) => return Ok(run_id),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::Workflow(
        "could not allocate a unique workflow runId".to_string(),
    ))
}

/// The task id the model sees for a run.
pub fn create_task_id(run_id: &str) -> String {
    format!("local_workflow_{run_id}")
}

/// Atomically write `value` as pretty JSON: write a sibling temp, then rename.
fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, serde_json::to_string_pretty(value)?)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Persist the snapshot the resume path reads back.
fn write_snapshot(
    store: &Path,
    run_id: &str,
    name: &str,
    args: &Value,
    task: &Arc<Mutex<WorkflowTask>>,
) -> Result<()> {
    let task = task.lock().unwrap().serialize();
    write_json(
        &store.join(format!("{run_id}.json")),
        &json!({
            "runId": run_id,
            "workflowName": name,
            "args": args,
            "task": task,
        }),
    )
}

/// Read a previous run's snapshot.
fn read_snapshot(store: &Path, run_id: &str) -> Result<Value> {
    let path = store.join(format!("{run_id}.json"));
    if !path.exists() {
        return Err(Error::Workflow(format!(
            "resume snapshot not found for {run_id}"
        )));
    }
    let text = std::fs::read_to_string(&path)?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| Error::Workflow(format!("invalid resume snapshot for {run_id}")))?;
    if !value.is_object() {
        return Err(Error::Workflow(format!(
            "invalid resume snapshot for {run_id}"
        )));
    }
    Ok(value)
}

/// Remember the most recent run so `resume` can find it.
fn save_last_run(store: &Path, run_id: &str) -> Result<()> {
    std::fs::write(store.join(LAST_RUN_FILE), run_id)?;
    Ok(())
}

/// The most recent run id under `workspace`, if any.
pub fn last_run(workspace: &Path) -> Option<String> {
    let path = workspace.join(WORKFLOWS_DIR).join(LAST_RUN_FILE);
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Report one lifecycle event to the host listener.
fn emit_event(listener: &Option<WorkflowListener>, run_id: &str, kind: &str, message: String) {
    if let Some(listener) = listener {
        listener(&WorkflowEvent {
            run_id: run_id.to_string(),
            kind: kind.to_string(),
            message,
        });
    }
}

// -- Tool ----------------------------------------------------------------

/// `Workflow`: run (or resume) a saved procedure by name.
pub struct WorkflowTool {
    registry: WorkflowRegistry,
}

impl WorkflowTool {
    /// Bind the tool to a registry.
    pub fn new(registry: WorkflowRegistry) -> Self {
        WorkflowTool { registry }
    }

    /// The registry this tool resolves names against.
    pub fn registry(&self) -> &WorkflowRegistry {
        &self.registry
    }
}

#[async_trait]
impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        WORKFLOW_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Run a saved workflow by name: pass a workflow name and its args to run a \
         scripted, resumable orchestration instead of driving each step yourself. \
         Pass resume_from_run_id to continue an interrupted run."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Saved workflow slug."
                },
                "args": {
                    "type": "object",
                    "description": "Workflow inputs."
                },
                "resume_from_run_id": {
                    "type": "string",
                    "description": "Continue an earlier run (`wf_<name>_<16 hex>`)."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<String> {
        let name = required_str(&input, "name")?;
        let args = input.get("args").cloned();
        let resume = input
            .get("resume_from_run_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let envelope = self
            .registry
            .run(ctx.workspace(), name, args, resume)
            .await?;
        Ok(serde_json::to_string_pretty(&envelope)?)
    }
}

/// Register the `Workflow` tool.
pub fn register_workflow_tools(registry: &mut ToolRegistry, workflows: WorkflowRegistry) {
    registry.register(WorkflowTool::new(workflows));
}

// -- Sample workflow -----------------------------------------------------

/// `findings` schema the sample review workflow asks each auditor for.
pub fn findings_schema() -> Value {
    json!({
        "type": "object",
        "required": ["findings"],
        "properties": {
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["title", "severity"],
                    "properties": {
                        "title": { "type": "string" },
                        "severity": { "type": "string", "enum": ["high", "medium", "low"] }
                    }
                }
            }
        }
    })
}

/// `verdict` schema the sample verify stage asks each verifier for.
pub fn verdict_schema() -> Value {
    json!({
        "type": "object",
        "required": ["isReal", "reason"],
        "properties": {
            "isReal": { "type": "boolean" },
            "reason": { "type": "string" }
        }
    })
}

/// The dimensions the sample workflow audits.
pub const REVIEW_DIMENSIONS: [&str; 4] = ["correctness", "security", "performance", "style"];

/// A small, deliberately vulnerable change the demo reviews.
pub const DEMO_CHANGES: &str = "def load_user(user_id):\n    query = f\"SELECT * FROM users WHERE id = {user_id}\"\n    return db.execute(query).fetchone()\n";

/// Metadata for the built-in `review-changes` workflow.
pub fn review_changes_meta() -> WorkflowMeta {
    WorkflowMeta::new(
        "review-changes",
        "Review changed files across dimensions, then verify each finding",
        vec!["Review".to_string(), "Verify".to_string()],
    )
    .expect("the built-in meta is valid")
}

/// A registry with the built-in sample workflow registered.
pub fn default_registry() -> Result<WorkflowRegistry> {
    let registry = WorkflowRegistry::new();
    registry.register(review_changes_meta(), review_changes_script())?;
    Ok(registry)
}

/// Severity ordering used to sort confirmed findings: high, medium, low, other.
fn severity_rank(finding: &Value) -> u8 {
    match finding.get("severity").and_then(Value::as_str) {
        Some("high") => 0,
        Some("medium") => 1,
        Some("low") => 2,
        _ => 3,
    }
}

/// The built-in `review-changes` script: a `pipeline` over dimensions
/// (audit -> verify-each) that keeps only the findings a verifier confirms.
/// The plan is host code, never a chat turn.
pub fn review_changes_script() -> WorkflowScript {
    Arc::new(|ctx: ExecutionContext, args: Value| {
        Box::pin(async move {
            ctx.phase("Review");
            let changes = args
                .get("changes")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let review_input = Arc::new(if changes.is_empty() {
                "No change context was supplied.".to_string()
            } else {
                changes
            });
            let dimensions: Vec<Value> = REVIEW_DIMENSIONS
                .iter()
                .map(|dimension| Value::String((*dimension).to_string()))
                .collect();

            let audit_input = Arc::clone(&review_input);
            let audit: PipelineStage = Arc::new(
                move |ctx: ExecutionContext, _previous: Value, dimension: Value, _index: usize| {
                    let review_input = Arc::clone(&audit_input);
                    Box::pin(async move {
                        let dimension = dimension.as_str().unwrap_or_default().to_string();
                        let prompt = format!(
                            "Review this change context for {dimension} issues. Report only \
                             issues supported by the supplied text.\n\n{review_input}"
                        );
                        let output = ctx
                            .agent(
                                prompt,
                                Some(findings_schema()),
                                Some(format!("audit:{dimension}")),
                                Some("Review".to_string()),
                            )
                            .await?;
                        let findings = output.get("findings").cloned().unwrap_or_else(|| json!([]));
                        Ok(json!({ "dimension": dimension, "findings": findings }))
                    })
                },
            );

            let verify_input = Arc::clone(&review_input);
            let verify: PipelineStage = Arc::new(
                move |ctx: ExecutionContext, audited: Value, dimension: Value, _index: usize| {
                    let review_input = Arc::clone(&verify_input);
                    Box::pin(async move {
                        ctx.phase("Verify");
                        let dimension = dimension.as_str().unwrap_or_default().to_string();
                        let findings = audited
                            .get("findings")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        let mut thunks: Vec<BoxWorkflowFuture> = Vec::with_capacity(findings.len());
                        for finding in &findings {
                            let title = finding
                                .get("title")
                                .and_then(Value::as_str)
                                .unwrap_or("finding")
                                .to_string();
                            let prompt = format!(
                                "Adversarially verify this {dimension} finding against the \
                                 supplied change context.\n\nChange context:\n{review_input}\n\n\
                                 Finding:\n{}",
                                canonical_json(finding)
                            );
                            thunks.push(ctx.agent(
                                prompt,
                                Some(verdict_schema()),
                                Some(format!("verify:{dimension}:{title}")),
                                Some("Verify".to_string()),
                            ));
                        }
                        let verdicts = ctx.parallel(thunks).await?;
                        let confirmed: Vec<Value> = findings
                            .iter()
                            .zip(verdicts.iter())
                            .filter(|(_finding, verdict)| {
                                verdict
                                    .get("isReal")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false)
                            })
                            .map(|(finding, _verdict)| finding.clone())
                            .collect();
                        Ok(json!({ "dimension": dimension, "confirmed": confirmed }))
                    })
                },
            );

            let results = ctx.pipeline(dimensions, vec![audit, verify]).await?;
            let mut confirmed: Vec<Value> = Vec::new();
            for result in &results {
                let dimension = result
                    .get("dimension")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if let Some(items) = result.get("confirmed").and_then(Value::as_array) {
                    for finding in items {
                        let mut entry = finding.clone();
                        if let Some(object) = entry.as_object_mut() {
                            object
                                .insert("dimension".to_string(), Value::String(dimension.clone()));
                        }
                        confirmed.push(entry);
                    }
                }
            }
            confirmed.sort_by_key(severity_rank);
            ctx.log(&format!("confirmed {} real finding(s)", confirmed.len()));
            Ok(json!({ "confirmed": confirmed }))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    fn sample_args() -> Value {
        json!({ "changes": DEMO_CHANGES, "budget": Value::Null })
    }

    #[test]
    fn run_ids_and_names_follow_the_contract() {
        assert!(valid_workflow_name("review-changes"));
        assert!(valid_workflow_name("a.b_c-1"));
        assert!(!valid_workflow_name(""));
        assert!(!valid_workflow_name("-leading"));
        assert!(!valid_workflow_name("has space"));
        assert!(valid_run_id("wf_review-changes_0123456789abcdef"));
        assert!(!valid_run_id("wf_review-changes_0123"));
        assert!(!valid_run_id("wf_review-changes_0123456789ABCDEF"));
    }

    #[test]
    fn schema_validation_covers_the_subset() {
        let schema = findings_schema();
        assert!(validate_schema(&schema, &json!({ "findings": [] })).is_ok());
        assert!(validate_schema(&schema, &json!({})).is_err());
        let bad = validate_schema(
            &schema,
            &json!({ "findings": [{ "title": "t", "severity": "urgent" }] }),
        );
        assert!(bad.unwrap_err().contains("severity"));
    }

    #[test]
    fn journal_replays_by_semantic_key() {
        let dir = workspace();
        let key = WorkflowJournal::key(
            "agent",
            "audit:security",
            "prompt",
            Some(&findings_schema()),
        );
        let again = WorkflowJournal::key(
            "agent",
            "audit:security",
            "prompt",
            Some(&findings_schema()),
        );
        assert_eq!(key, again, "the key is order-independent");

        let fresh = WorkflowJournal::open(dir.path(), "wf_demo_0123456789abcdef", false).unwrap();
        assert!(fresh.cached(&key).is_none());
        fresh.record(&key, &json!({ "findings": [] })).unwrap();
        assert_eq!(fresh.cached(&key), Some(json!({ "findings": [] })));

        let resumed = WorkflowJournal::open(dir.path(), "wf_demo_0123456789abcdef", true).unwrap();
        assert_eq!(resumed.cached(&key), Some(json!({ "findings": [] })));
        assert!(WorkflowJournal::open(dir.path(), "wf_other_aaaaaaaaaaaaaaaa", true).is_err());
    }

    #[test]
    fn budget_refuses_to_overspend() {
        let budget = Budget::new(Some(10));
        budget.add(4).unwrap();
        budget.add(6).unwrap();
        assert_eq!(budget.spent(), 10);
        assert_eq!(budget.remaining(), Some(0));
        assert!(budget.add(1).is_err());
        assert_eq!(Budget::new(None).remaining(), None);
    }

    #[tokio::test]
    async fn mock_runner_satisfies_the_findings_schema() {
        let output = MockAgentRunner
            .run(
                "review security",
                Some(&findings_schema()),
                Some("audit:security"),
            )
            .await
            .unwrap();
        assert!(validate_schema(&findings_schema(), &output.value).is_ok());
        assert!(output.tokens > 0);
    }

    #[tokio::test]
    async fn sample_workflow_runs_the_pipeline() {
        let dir = workspace();
        let registry = default_registry().unwrap();
        let envelope = registry
            .run(dir.path(), "review-changes", Some(sample_args()), None)
            .await
            .unwrap();

        assert_eq!(envelope["task"]["status"], json!("completed"));
        let agents = envelope["task"]["usage"]["agents"].as_u64().unwrap();
        assert!(agents >= 4, "one audit per dimension: {agents}");
        assert!(envelope["result"]["confirmed"].is_array());

        let progress = envelope["task"]["progress"].as_array().unwrap();
        let kinds: Vec<&str> = progress
            .iter()
            .filter_map(|event| event["type"].as_str())
            .collect();
        assert!(kinds.contains(&"workflow_phase"));
        assert!(kinds.contains(&"workflow_agent"));

        let run_id = envelope["task"]["runId"].as_str().unwrap();
        assert!(dir
            .path()
            .join(WORKFLOWS_DIR)
            .join(format!("{run_id}.output.json"))
            .exists());
        assert_eq!(last_run(dir.path()).as_deref(), Some(run_id));
    }

    #[tokio::test]
    async fn resume_replays_journalled_steps() {
        let dir = workspace();
        let registry = default_registry().unwrap();
        let first = registry
            .run(dir.path(), "review-changes", Some(sample_args()), None)
            .await
            .unwrap();
        let run_id = first["task"]["runId"].as_str().unwrap().to_string();

        let second = registry
            .run(
                dir.path(),
                "review-changes",
                Some(sample_args()),
                Some(run_id),
            )
            .await
            .unwrap();
        assert_eq!(second["task"]["status"], json!("completed"));
        assert_eq!(
            second["task"]["usage"]["agents"],
            json!(0),
            "every step replays from the journal"
        );
        assert_eq!(first["result"], second["result"]);
    }

    #[tokio::test]
    async fn lifecycle_events_reach_the_listener() {
        let dir = workspace();
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let registry =
            default_registry()
                .unwrap()
                .with_listener(Arc::new(move |event: &WorkflowEvent| {
                    sink.lock()
                        .unwrap()
                        .push((event.kind.clone(), event.run_id.clone()));
                }));
        registry
            .run(dir.path(), "review-changes", Some(sample_args()), None)
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        let kinds: Vec<&str> = seen.iter().map(|(kind, _)| kind.as_str()).collect();
        assert!(kinds.contains(&"async_launched"));
        assert!(kinds.contains(&"task_started"));
        assert!(kinds.contains(&"task_notification"));
        assert!(kinds.contains(&"workflow_phase"));
    }

    #[tokio::test]
    async fn resume_rejects_a_mismatched_workflow() {
        let dir = workspace();
        let registry = default_registry().unwrap();
        let first = registry
            .run(dir.path(), "review-changes", Some(sample_args()), None)
            .await
            .unwrap();
        let run_id = first["task"]["runId"].as_str().unwrap().to_string();

        let other = WorkflowRegistry::new();
        other
            .register(
                WorkflowMeta::new("other", "another procedure", vec![]).unwrap(),
                Arc::new(|_ctx, _args| Box::pin(async { Ok(json!({})) })),
            )
            .unwrap();
        let error = other
            .run(dir.path(), "other", Some(json!({})), Some(run_id))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("does not match workflow meta"));
    }

    #[tokio::test]
    async fn a_tiny_budget_fails_the_run_without_overspending() {
        let dir = workspace();
        let registry = default_registry().unwrap();
        let envelope = registry
            .run(
                dir.path(),
                "review-changes",
                Some(json!({ "changes": "x", "budget": 0 })),
                None,
            )
            .await
            .unwrap();
        assert_eq!(envelope["task"]["status"], json!("failed"));
        assert_eq!(envelope["task"]["usage"]["agents"], json!(0));
        let message = envelope["result"]["error"].as_str().unwrap();
        assert!(message.contains("token budget exceeded"), "{message}");
    }

    #[tokio::test]
    async fn unknown_workflows_and_bad_args_are_refused() {
        let dir = workspace();
        let registry = default_registry().unwrap();
        assert!(registry.run(dir.path(), "nope", None, None).await.is_err());
        assert!(registry
            .run(
                dir.path(),
                "review-changes",
                Some(json!("not an object")),
                None
            )
            .await
            .is_err());
        assert!(registry
            .run(
                dir.path(),
                "review-changes",
                None,
                Some("not-a-run-id".to_string())
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_bad_schema_answer_is_re_asked_once() {
        struct RetryRunner {
            calls: AtomicUsize,
        }

        #[async_trait]
        impl AgentRunner for RetryRunner {
            async fn run(
                &self,
                _prompt: &str,
                schema: Option<&Value>,
                _label: Option<&str>,
            ) -> Result<RunnerOutput> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                let value = if schema.is_some() && call == 0 {
                    json!("not an object")
                } else {
                    json!({ "findings": [] })
                };
                Ok(RunnerOutput { value, tokens: 5 })
            }
        }

        let dir = workspace();
        let registry = WorkflowRegistry::new().with_runner(Arc::new(RetryRunner {
            calls: AtomicUsize::new(0),
        }));
        registry
            .register(
                WorkflowMeta::new("retry-demo", "asks once, then asks again", vec![]).unwrap(),
                Arc::new(|ctx: ExecutionContext, _args: Value| {
                    Box::pin(async move {
                        ctx.agent("audit", Some(findings_schema()), None, None)
                            .await
                    })
                }),
            )
            .unwrap();
        let envelope = registry
            .run(dir.path(), "retry-demo", None, None)
            .await
            .unwrap();
        assert_eq!(envelope["task"]["status"], json!("completed"));
        assert_eq!(envelope["task"]["usage"]["agents"], json!(1));
        assert_eq!(envelope["task"]["usage"]["tokens"], json!(10));
        assert_eq!(envelope["result"], json!({ "findings": [] }));
    }

    #[tokio::test]
    async fn parallel_keeps_input_order() {
        let dir = workspace();
        let registry = WorkflowRegistry::new();
        registry
            .register(
                WorkflowMeta::new("ordering", "three parallel steps", vec![]).unwrap(),
                Arc::new(|ctx: ExecutionContext, _args: Value| {
                    Box::pin(async move {
                        let thunks: Vec<BoxWorkflowFuture> = ["a", "b", "c"]
                            .iter()
                            .map(|label| ctx.agent("step", None, Some((*label).to_string()), None))
                            .collect();
                        let values = ctx.parallel(thunks).await?;
                        Ok(json!({ "values": values }))
                    })
                }),
            )
            .unwrap();
        let envelope = registry
            .run(dir.path(), "ordering", None, None)
            .await
            .unwrap();
        assert_eq!(
            envelope["result"]["values"],
            json!(["[mock] a", "[mock] b", "[mock] c"])
        );
    }

    #[tokio::test]
    async fn the_tool_returns_the_launch_envelope() {
        let dir = workspace();
        let mut tools = ToolRegistry::new();
        register_workflow_tools(&mut tools, default_registry().unwrap());
        let tool = tools.get(WORKFLOW_TOOL_NAME).expect("registered");
        let ctx = ToolContext::new(dir.path());
        let output = tool
            .call(
                &ctx,
                json!({ "name": "review-changes", "args": sample_args() }),
            )
            .await
            .unwrap();
        let envelope: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(envelope["task"]["taskType"], json!("local_workflow"));
        assert_eq!(
            envelope["launched"]["workflowName"],
            json!("review-changes")
        );
        assert_eq!(
            envelope["launched"]["taskId"],
            json!(create_task_id(envelope["task"]["runId"].as_str().unwrap()))
        );
    }
}
