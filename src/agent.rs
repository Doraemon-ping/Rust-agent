//! The agent loop.
//!
//! ```text
//!   loop {
//!       response = model(messages, tools)
//!       messages += response
//!       if response has no tool calls {
//!           if a Stop hook asks to continue { keep going } else { stop }
//!       }
//!       messages += run(each tool call)      // gated by PreToolUse hooks
//!   }
//! ```
//!
//! The loop itself knows nothing about permissions or logging: those live in
//! hooks (see [`crate::hooks`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::background::{placeholder, should_run_background, BackgroundManager};
use crate::compact::{
    is_context_overflow, CompactConfig, CompactTool, Compactor, COMPACT_TOOL_NAME,
};
use crate::cron::{register_cron_tools, CronJob, CronScheduler};
use crate::error::{Error, Result};
use crate::hooks::{Hook, Hooks};
use crate::llm::{CompletionRequest, CompletionResponse, LlmClient, StopReason, ToolDefinition};
use crate::mcp::{register_mcp_tools, McpPermissionHook, McpRegistry, MCP_TOOL_PREFIX};
use crate::memory::{CatalogRecall, MemoryConfig, MemoryManager, MemoryRecall};
use crate::message::{ContentBlock, Message, Role, ToolCall};
use crate::permission::{Approver, AutoApprover, PermissionHook, PermissionPolicy};
use crate::recovery::{
    classify, is_retryable, ErrorClass, RecoveryConfig, RecoveryState, CONTINUATION_PROMPT,
};
use crate::skills::SkillLoader;
use crate::team::{register_team_tools, Team};
use crate::tools::{BashTool, LoadSkillTool, ToolContext, ToolRegistry, TODO_TOOL_NAME};
use crate::workflow::{register_workflow_tools, WorkflowEvent, WorkflowRegistry};

/// Reminder injected when the model goes several rounds without updating its
/// todo list (s05).
const TODO_REMINDER: &str = "<reminder>Update your todos.</reminder>";

/// Something worth reporting while the loop runs.
#[derive(Debug)]
pub enum AgentEvent<'a> {
    /// A model turn began (zero-based step index).
    Step(usize),
    /// The model produced text.
    AssistantText(&'a str),
    /// The model asked for a tool.
    ToolCall { name: &'a str, input: &'a Value },
    /// A tool finished.
    ToolResult {
        name: &'a str,
        output: &'a str,
        is_error: bool,
    },
    /// A hook refused a call before (or instead of) running it.
    Blocked { name: &'a str, reason: &'a str },
    /// The history was compacted (`kind` is the step: `"spill"`, `"snip"`,
    /// `"micro"`, `"fit"`, `"Compacted"`, or `"reactive"`).
    Compacted {
        kind: &'a str,
        transcript: Option<&'a Path>,
    },
    /// Relevant memory records were folded into this turn's system prompt.
    MemoryRecalled { count: usize },
    /// The post-turn pass stored memories (and possibly consolidated them).
    MemoryStored { stored: usize, consolidated: usize },
    /// A `bash` call was dispatched to a background thread (s11).
    BackgroundStarted { task_id: &'a str },
    /// Completed background results were injected into this turn (s11).
    BackgroundCollected { count: usize },
    /// Due cron jobs were delivered as `[Scheduled]` user messages (s12).
    CronDelivered { count: usize },
    /// Teammate events were folded into this turn (s13).
    TeamEventDelivered { count: usize },
    /// An MCP server was connected and its tools discovered (s14).
    McpConnected { server: &'a str, tools: usize },
    /// A transient model failure is being retried (s15).
    ModelRecovered {
        kind: &'a str,
        attempt: usize,
        delay_ms: u64,
    },
    /// Repeated overloads switched the turn to the fallback model (s15).
    ModelFellBack { model: &'a str },
    /// A truncated response is retried with a bigger token budget (s15).
    MaxTokensEscalated { max_tokens: u32 },
    /// A truncated, tool-less response asked the model to continue (s15).
    ContinuationRequested { count: usize },
    /// A workflow run reported lifecycle or progress (s16).
    WorkflowReported {
        run_id: &'a str,
        kind: &'a str,
        message: &'a str,
    },
}

/// Receives [`AgentEvent`]s. Implement to render progress (see the CLI) or to
/// add telemetry. Every method has a no-op default.
pub trait AgentObserver: Send + Sync {
    /// Handle one event.
    fn on_event(&self, _event: AgentEvent<'_>) {}
}

/// An observer that ignores everything.
pub struct NoopObserver;

impl AgentObserver for NoopObserver {}

/// The runtime: a model client, a tool registry, and a set of hooks wired
/// into one loop.
pub struct Agent {
    client: Arc<dyn LlmClient>,
    tools: Arc<ToolRegistry>,
    compactor: Option<Arc<Compactor>>,
    memory: Option<Arc<MemoryManager>>,
    ctx: ToolContext,
    hooks: Hooks,
    observer: Arc<dyn AgentObserver>,
    background: Option<BackgroundManager>,
    cron: Option<CronScheduler>,
    team: Option<Team>,
    mcp: Option<McpRegistry>,
    workflow: Option<WorkflowRegistry>,
    recovery: Option<RecoveryConfig>,
    interactive: bool,
    system_prompt: String,
    max_tokens: u32,
    max_steps: usize,
    todo_reminder_after: usize,
}

impl Agent {
    /// Start building an agent.
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// The tool context (workspace) this agent operates in.
    pub fn context(&self) -> &ToolContext {
        &self.ctx
    }

    /// The registered hooks.
    pub fn hooks(&self) -> &Hooks {
        &self.hooks
    }

    /// The compactor, when context compaction is enabled.
    pub fn compactor(&self) -> Option<&Compactor> {
        self.compactor.as_deref()
    }

    /// The memory manager, when durable memory is enabled.
    pub fn memory(&self) -> Option<&MemoryManager> {
        self.memory.as_deref()
    }

    /// The background task manager, when background execution is enabled (s11).
    pub fn background(&self) -> Option<&BackgroundManager> {
        self.background.as_ref()
    }

    /// The cron scheduler, when scheduled prompts are enabled (s12).
    pub fn cron(&self) -> Option<&CronScheduler> {
        self.cron.as_ref()
    }

    /// The team runtime, when teammates are enabled (s13).
    pub fn team(&self) -> Option<&Team> {
        self.team.as_ref()
    }

    /// The MCP registry, when external tools are enabled (s14).
    pub fn mcp(&self) -> Option<&McpRegistry> {
        self.mcp.as_ref()
    }

    /// The model-call recovery policy, when it is enabled (s15).
    pub fn recovery(&self) -> Option<&RecoveryConfig> {
        self.recovery.as_ref()
    }

    /// The workflow registry, when saved procedures are enabled (s16).
    pub fn workflow(&self) -> Option<&WorkflowRegistry> {
        self.workflow.as_ref()
    }

    /// Whether this agent may pause and ask the user for approval (s15).
    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    /// Run `UserPromptSubmit` hooks and return the prompt to send, enriched
    /// with any hook-provided context.
    pub fn prepare_prompt(&self, prompt: &str) -> String {
        match self.hooks.user_prompt_submit(prompt) {
            Some(context) => format!("{context}\n\n{prompt}"),
            None => prompt.to_string(),
        }
    }

    /// Run the loop until the model stops asking for tools.
    ///
    /// `history` is the live conversation. The loop appends to it, so callers
    /// keep multi-turn context across `run_turn` calls.
    pub async fn run_turn(&self, history: &mut Vec<Message>) -> Result<()> {
        let mut rounds_since_todo = 0usize;
        let active_request = active_request(history);
        let mut reactive_retries = 0usize;

        // s13: teammates speak to the Lead over mailboxes rather than through
        // its context; fold whatever arrived into this turn.
        if let Some(team) = &self.team {
            if let Ok(events) = team.consume_lead_inbox() {
                if !events.is_empty() {
                    self.observer.on_event(AgentEvent::TeamEventDelivered {
                        count: events.len(),
                    });
                    history.push(Message::user_text(Team::format_team_events(&events)));
                }
            }
        }

        // s12: a matched cron job becomes a `[Scheduled]` user message for this
        // turn. It is acknowledged only once the model has accepted it, and put
        // back (with the message removed) if the call fails.
        let rollback_to = history.len();
        let mut cron_unacked: Vec<CronJob> = Vec::new();
        if let Some(cron) = &self.cron {
            cron.poll(CronScheduler::now())?;
            let due = cron.take_due();
            if !due.is_empty() {
                self.observer
                    .on_event(AgentEvent::CronDelivered { count: due.len() });
                let blocks: Vec<ContentBlock> = due
                    .iter()
                    .map(|job| ContentBlock::text(format!("[Scheduled] {}", job.prompt)))
                    .collect();
                history.push(Message::user_blocks(blocks));
                cron_unacked = due;
            }
        }

        // s09: recall relevant records once per turn; the system prompt is
        // fixed for the whole turn.
        let system_prompt = match &self.memory {
            Some(memory) => {
                let recalled = memory.recall(history).await.unwrap_or_default();
                if recalled.count > 0 {
                    self.observer.on_event(AgentEvent::MemoryRecalled {
                        count: recalled.count,
                    });
                }
                memory.system_prompt(&self.system_prompt, &recalled.section)
            }
            None => self.system_prompt.clone(),
        };
        // s15: per-turn recovery bookkeeping (one escalation, bounded
        // continuations) and the token budget this turn is currently using.
        let mut recovery = RecoveryState::new();
        let mut max_tokens = self.max_tokens;

        for step in 0..self.max_steps {
            self.observer.on_event(AgentEvent::Step(step));

            // s11: completed background results ride in on a later turn, as
            // independent `<task_notification>` blocks (never on the original
            // tool_use_id, which the placeholder already answered).
            if let Some(background) = &self.background {
                let notifications = background.collect();
                if !notifications.is_empty() {
                    self.observer.on_event(AgentEvent::BackgroundCollected {
                        count: notifications.len(),
                    });
                    inject_notifications(history, notifications);
                }
            }

            if let Some(compactor) = &self.compactor {
                compactor.prepare(history, &active_request).await?;
            }

            // s14: reassemble the tool pool every round, so a server connected
            // on the previous round is visible to the model on this one.
            let mut tools = self.tools.definitions();
            // ...and announce the connected servers alongside it, so the model
            // does not have to guess which `mcp__*` tools exist.
            let system_prompt = match &self.mcp {
                Some(mcp) => {
                    let section = mcp.prompt_section();
                    if section.is_empty() {
                        system_prompt.clone()
                    } else {
                        format!("{system_prompt}{section}")
                    }
                }
                None => system_prompt.clone(),
            };
            if let Some(mcp) = &self.mcp {
                tools.extend(mcp.tool_definitions());
            }

            let response = match self
                .complete_with_recovery(
                    None,
                    &system_prompt,
                    history,
                    &tools,
                    max_tokens,
                    &mut recovery,
                )
                .await
            {
                Ok(response) => {
                    if !cron_unacked.is_empty() {
                        if let Some(cron) = &self.cron {
                            cron.acknowledge(&cron_unacked)?;
                        }
                        cron_unacked.clear();
                    }
                    reactive_retries = 0;
                    response
                }
                Err(error) => match &self.compactor {
                    Some(compactor)
                        if is_context_overflow(&error)
                            && reactive_retries < compactor.config().max_reactive_retries =>
                    {
                        compactor.reactive_compact(history, &active_request).await?;
                        reactive_retries += 1;
                        continue;
                    }
                    _ => {
                        if !cron_unacked.is_empty() {
                            if let Some(cron) = &self.cron {
                                cron.restore(&cron_unacked);
                            }
                            history.truncate(rollback_to);
                        }
                        return Err(error);
                    }
                },
            };

            // s15: a response cut off by the token budget is retried once with
            // a bigger budget; if it is still truncated and there is nothing to
            // run, keep what we have and ask the model to continue.
            if response.stop_reason() == StopReason::MaxTokens {
                if let Some(config) = &self.recovery {
                    if !recovery.has_escalated && config.escalated_max_tokens > max_tokens {
                        recovery.has_escalated = true;
                        max_tokens = config.escalated_max_tokens;
                        self.observer
                            .on_event(AgentEvent::MaxTokensEscalated { max_tokens });
                        continue;
                    }
                    if response.tool_calls().is_empty()
                        && recovery.continuation_count < config.max_continuation_retries
                    {
                        recovery.continuation_count += 1;
                        self.observer.on_event(AgentEvent::ContinuationRequested {
                            count: recovery.continuation_count,
                        });
                        for block in &response.content {
                            if let Some(text) = block.as_text() {
                                self.observer.on_event(AgentEvent::AssistantText(text));
                            }
                        }
                        history.push(Message::assistant(response.content));
                        history.push(Message::user_text(CONTINUATION_PROMPT));
                        continue;
                    }
                }
            }
            // A complete response resets the per-turn recovery budget.
            max_tokens = self.max_tokens;
            recovery.has_escalated = false;

            for block in &response.content {
                if let Some(text) = block.as_text() {
                    self.observer.on_event(AgentEvent::AssistantText(text));
                }
            }

            let calls = response.tool_calls();
            history.push(Message::assistant(response.content));

            if calls.is_empty() {
                // Stop hook: a hook may inject a message and keep us going.
                if let Some(forced) = self.hooks.stop(history) {
                    history.push(Message::user_text(forced));
                    continue;
                }
                if let Some(memory) = &self.memory {
                    let outcome = memory.after_turn(history).await;
                    if outcome.stored > 0 || outcome.consolidated > 0 {
                        self.observer.on_event(AgentEvent::MemoryStored {
                            stored: outcome.stored,
                            consolidated: outcome.consolidated,
                        });
                    }
                }
                return Ok(());
            }

            let mut results = Vec::with_capacity(calls.len() + 1);
            let mut used_todo = false;
            let mut compact_requested = false;
            for call in calls {
                let is_todo = call.name == TODO_TOOL_NAME;
                compact_requested |= call.name == COMPACT_TOOL_NAME;
                let (block, executed) = self.run_tool(call).await;
                used_todo |= is_todo && executed;
                results.push(block);
            }

            // s05: nudge the model if it has not touched its todos in a while.
            if self.todo_reminder_after > 0 {
                rounds_since_todo = if used_todo { 0 } else { rounds_since_todo + 1 };
                if rounds_since_todo >= self.todo_reminder_after {
                    results.push(ContentBlock::text(TODO_REMINDER));
                    rounds_since_todo = 0;
                }
            }

            history.push(Message::user_blocks(results));

            if compact_requested {
                if let Some(compactor) = &self.compactor {
                    compactor
                        .compact_history(history, &active_request, "Compacted")
                        .await?;
                }
            }
        }

        Err(Error::MaxIterations(self.max_steps))
    }

    /// One-shot convenience: run a fresh conversation and return the final
    /// assistant text.
    pub async fn ask(&self, prompt: impl Into<String>) -> Result<String> {
        let prompt = self.prepare_prompt(&prompt.into());
        let mut history = vec![Message::user_text(prompt)];
        self.run_turn(&mut history).await?;
        Ok(history.last().map(Message::text).unwrap_or_default())
    }

    /// Run one tool call, returning the result block and whether the tool
    /// actually executed (a hook may have blocked it).
    async fn run_tool(&self, call: ToolCall) -> (ContentBlock, bool) {
        self.observer.on_event(AgentEvent::ToolCall {
            name: &call.name,
            input: &call.input,
        });

        // PreToolUse hooks: the first hook to return a reason blocks the call.
        if let Some(reason) = self.hooks.pre_tool_use(&call) {
            self.observer.on_event(AgentEvent::Blocked {
                name: &call.name,
                reason: &reason,
            });
            self.observer.on_event(AgentEvent::ToolResult {
                name: &call.name,
                output: &reason,
                is_error: true,
            });
            return (ContentBlock::tool_result(call.id, reason, true), false);
        }

        // s11: an explicit `run_in_background` hands the command to a worker
        // thread and answers this call at once with a placeholder result.
        if let Some(background) = &self.background {
            if should_run_background(&call.name, &call.input) {
                let command = call
                    .input
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let (output, is_error) = match background.start(self.ctx.workspace(), command) {
                    Ok(task_id) => {
                        self.observer
                            .on_event(AgentEvent::BackgroundStarted { task_id: &task_id });
                        (placeholder(&task_id), false)
                    }
                    Err(error) => (format!("Error: {error}"), true),
                };
                self.hooks.post_tool_use(&call, &output);
                self.observer.on_event(AgentEvent::ToolResult {
                    name: &call.name,
                    output: &output,
                    is_error,
                });
                return (ContentBlock::tool_result(call.id, output, is_error), true);
            }
        }

        let (output, is_error) = self.dispatch(&call).await;
        self.hooks.post_tool_use(&call, &output);
        self.observer.on_event(AgentEvent::ToolResult {
            name: &call.name,
            output: &output,
            is_error,
        });
        (ContentBlock::tool_result(call.id, output, is_error), true)
    }

    /// s15: ask the model, backing off over transient provider failures.
    ///
    /// `429` and `529` are retried with exponential backoff (and jitter) up to
    /// the configured budget; repeated overloads switch the turn to the
    /// fallback model. Every other failure is returned untouched, so the
    /// caller can decide whether to compact and retry or roll the turn back.
    #[allow(clippy::too_many_arguments)]
    async fn complete_with_recovery(
        &self,
        model: Option<&str>,
        system: &str,
        history: &[Message],
        tools: &[ToolDefinition],
        max_tokens: u32,
        recovery: &mut RecoveryState,
    ) -> Result<CompletionResponse> {
        let Some(config) = &self.recovery else {
            return self
                .client
                .complete(CompletionRequest {
                    model,
                    system,
                    messages: history,
                    tools,
                    max_tokens,
                })
                .await;
        };

        let mut attempt = 0usize;
        loop {
            let result = self
                .client
                .complete(CompletionRequest {
                    model: recovery.current_model.as_deref().or(model),
                    system,
                    messages: history,
                    tools,
                    max_tokens,
                })
                .await;
            let error = match result {
                Ok(response) => {
                    recovery.consecutive_overloaded = 0;
                    return Ok(response);
                }
                Err(error) => error,
            };

            let class = classify(&error);
            if !is_retryable(class) || attempt + 1 >= config.max_retries {
                return Err(error);
            }
            if class == ErrorClass::Overloaded {
                if let Some(fallback) = config.note_overloaded(recovery) {
                    self.observer
                        .on_event(AgentEvent::ModelFellBack { model: &fallback });
                }
            }
            let delay = config.delay(attempt);
            self.observer.on_event(AgentEvent::ModelRecovered {
                kind: if class == ErrorClass::RateLimited {
                    "429"
                } else {
                    "529"
                },
                attempt: attempt + 1,
                delay_ms: delay.as_millis() as u64,
            });
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    async fn dispatch(&self, call: &ToolCall) -> (String, bool) {
        // s14: a discovered tool is not in the registry; it lives in the MCP
        // registry until the turn ends, so dispatch resolves it by prefix.
        if call.name.starts_with(MCP_TOOL_PREFIX) {
            if let Some(mcp) = &self.mcp {
                return match mcp.call_tool(&call.name, &call.input) {
                    Ok(output) => (output, false),
                    Err(error) => (format!("Error: {error}"), true),
                };
            }
        }
        match self.tools.execute(&self.ctx, call).await {
            Ok(output) => (output, false),
            Err(error) => (format!("Error: {error}"), true),
        }
    }
}

/// Fluent builder for [`Agent`].
#[derive(Default)]
pub struct AgentBuilder {
    client: Option<Arc<dyn LlmClient>>,
    tools: Option<ToolRegistry>,
    skills: Option<Arc<SkillLoader>>,
    compaction: Option<CompactConfig>,
    memory: Option<MemoryConfig>,
    memory_recall: Option<Arc<dyn MemoryRecall>>,
    workspace: Option<PathBuf>,
    policy: Option<PermissionPolicy>,
    approver: Option<Arc<dyn Approver>>,
    hooks: Vec<Arc<dyn Hook>>,
    observer: Option<Arc<dyn AgentObserver>>,
    background: Option<BackgroundManager>,
    cron: Option<CronScheduler>,
    team: Option<Team>,
    mcp: Option<McpRegistry>,
    workflow: Option<WorkflowRegistry>,
    recovery: Option<RecoveryConfig>,
    interactive: Option<bool>,
    system_prompt: Option<String>,
    max_tokens: Option<u32>,
    max_steps: Option<usize>,
    todo_reminder_after: Option<usize>,
}

impl AgentBuilder {
    /// Set the model client.
    pub fn client(mut self, client: impl LlmClient + 'static) -> Self {
        self.client = Some(Arc::new(client));
        self
    }

    /// Set the model client from a shared handle.
    pub fn shared_client(mut self, client: Arc<dyn LlmClient>) -> Self {
        self.client = Some(client);
        self
    }

    /// Use an explicit tool registry (defaults to the built-in tools).
    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Enable skills (s07).
    ///
    /// The skill catalog is appended to the system prompt (default or custom)
    /// and `load_skill` is registered on the agent's tool registry. A loader
    /// that found nothing is ignored.
    pub fn skills(mut self, loader: impl Into<Arc<SkillLoader>>) -> Self {
        self.skills = Some(loader.into());
        self
    }

    /// Enable context compaction (s08).
    ///
    /// Registers the `compact` tool, runs the pipeline before every model call,
    /// and retries once when the provider rejects a prompt as too long.
    pub fn compaction(mut self, config: CompactConfig) -> Self {
        self.compaction = Some(config);
        self
    }

    /// Enable durable memory (s09).
    ///
    /// Recalls relevant records into the turn's system prompt, then extracts
    /// and consolidates durable knowledge once the turn stops. Records live in
    /// `<workspace>/.memory/`.
    pub fn memory(mut self, config: MemoryConfig) -> Self {
        self.memory = Some(config);
        self
    }

    /// Enable background Bash execution (s11).
    ///
    /// Advertises `run_in_background` on the `bash` tool and routes those
    /// calls through `manager`. A command the model flags runs on a worker
    /// thread; its result is injected as a `<task_notification>` on a later
    /// turn, so the loop never blocks on a slow command.
    pub fn background(mut self, manager: BackgroundManager) -> Self {
        self.background = Some(manager);
        self
    }

    /// Enable scheduled prompts (s12).
    ///
    /// Registers `schedule_cron`, `list_crons`, and `cancel_cron`, polls the
    /// scheduler at the start of every turn, and delivers due jobs as
    /// `[Scheduled] <prompt>` user messages.
    pub fn cron(mut self, scheduler: CronScheduler) -> Self {
        self.cron = Some(scheduler);
        self
    }

    /// Enable agent teams (s13).
    ///
    /// Registers the Lead's team tools and folds teammate events (results,
    /// idle notices, plan decisions) into the conversation at the start of
    /// every turn.
    pub fn team(mut self, team: Team) -> Self {
        self.team = Some(team);
        self
    }

    /// Enable MCP external tools (s14).
    ///
    /// Registers `connect_mcp` and gates the discovered `mcp__*` tools behind
    /// the host policy. Connected servers' tools join the model's tool pool on
    /// the next round, and their names are listed in the system prompt.
    pub fn mcp(mut self, registry: McpRegistry) -> Self {
        self.mcp = Some(registry);
        self
    }

    /// Enable saved workflows (s16).
    ///
    /// Registers the `Workflow` tool, which resolves a workflow *name* to a
    /// trusted host script and runs it (with `agent`/`parallel`/`pipeline`
    /// steps) under one tool call, journalling every step so an interrupted
    /// run can be resumed. Lifecycle and progress lines are forwarded to the
    /// agent's observer.
    pub fn workflow(mut self, registry: WorkflowRegistry) -> Self {
        self.workflow = Some(registry);
        self
    }

    /// Enable model-call recovery (s15).
    ///
    /// Retries `429`/`529` with exponential backoff, switches to the fallback
    /// model after repeated overloads, escalates the token budget once when a
    /// response is truncated, and asks the model to continue when a truncated
    /// turn has nothing left to run.
    pub fn recovery(mut self, config: RecoveryConfig) -> Self {
        self.recovery = Some(config);
        self
    }

    /// Whether this agent may pause and ask the user for approval (s15).
    ///
    /// Defaults to `true`. An asynchronous turn is built with `false`, so
    /// anything the policy would have asked about is denied instead of
    /// blocking on an unreachable prompt.
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = Some(interactive);
        self
    }

    /// Override the recall strategy (defaults to [`CatalogRecall`]).
    pub fn memory_recall(mut self, recall: impl MemoryRecall + 'static) -> Self {
        self.memory_recall = Some(Arc::new(recall));
        self
    }

    /// Set the workspace root (defaults to the current directory).
    pub fn workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    /// Use an explicit permission policy (defaults to the standard one).
    pub fn policy(mut self, policy: PermissionPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Set the approver used by the default permission hook.
    pub fn approver(mut self, approver: impl Approver + 'static) -> Self {
        self.approver = Some(Arc::new(approver));
        self
    }

    /// Register an extra hook.
    ///
    /// Hooks run in registration order, after the built-in permission hook.
    pub fn hook(mut self, hook: impl Hook + 'static) -> Self {
        self.hooks.push(Arc::new(hook));
        self
    }

    /// Set the event observer (defaults to [`NoopObserver`]).
    pub fn observer(mut self, observer: impl AgentObserver + 'static) -> Self {
        self.observer = Some(Arc::new(observer));
        self
    }

    /// Set the event observer from an already-shared handle.
    pub fn shared_observer(mut self, observer: Arc<dyn AgentObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Override the system prompt.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Response token budget per model call.
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Safety bound on loop iterations.
    pub fn max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = Some(max_steps);
        self
    }

    /// Number of tool rounds without a `todo_write` before the harness injects
    /// a reminder. Set to `0` to disable.
    pub fn todo_reminder_after(mut self, rounds: usize) -> Self {
        self.todo_reminder_after = Some(rounds);
        self
    }

    /// Finalize the agent.
    ///
    /// # Panics
    /// Panics if no client was provided.
    pub fn build(self) -> Agent {
        let workspace = self
            .workspace
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        // The permission pipeline is just the first hook, exactly as in s04.
        let mut hooks = Hooks::new();
        // s15: a turn with no terminal to prompt on denies instead of asking.
        let interactive = self.interactive.unwrap_or(true);
        let approver: Arc<dyn Approver> = self.approver.unwrap_or_else(|| Arc::new(AutoApprover));
        hooks.register(
            PermissionHook::new(
                self.policy
                    .unwrap_or_else(|| PermissionPolicy::new(workspace.clone())),
                Arc::clone(&approver),
            )
            .interactive(interactive),
        );
        // s14: discovered `mcp__*` tools answer to the host policy, not to the
        // server's own annotations.
        if let Some(registry) = &self.mcp {
            hooks.register(
                McpPermissionHook::new(registry.clone(), Arc::clone(&approver))
                    .interactive(interactive),
            );
        }
        for hook in self.hooks {
            hooks.register_arc(hook);
        }

        let mut tools = self.tools.unwrap_or_else(ToolRegistry::with_defaults);
        // s11: only advertise `run_in_background` when a manager can honour it.
        if self.background.is_some() {
            tools.register(BashTool::default().advertising_background());
        }
        // s12: the cron tools share the scheduler's job table.
        if let Some(scheduler) = &self.cron {
            register_cron_tools(&mut tools, scheduler.clone());
        }
        // s13: the Lead's team tools all share one runtime.
        if let Some(team) = &self.team {
            register_team_tools(&mut tools, team.clone());
        }
        // s14: `connect_mcp` shares the registry the tool pool reads from.
        if let Some(registry) = &self.mcp {
            register_mcp_tools(&mut tools, registry.clone());
        }
        // s16: the `Workflow` tool resolves saved scripts from the registry.
        if let Some(registry) = &self.workflow {
            register_workflow_tools(&mut tools, registry.clone());
        }
        if let Some(loader) = &self.skills {
            tools.register(LoadSkillTool::new(Arc::clone(loader)));
        }

        let mut system_prompt = self.system_prompt.unwrap_or_else(default_system_prompt);
        if let Some(loader) = &self.skills {
            if !loader.is_empty() {
                system_prompt = format!("{system_prompt}\n\n{}", loader.prompt_section());
            }
        }

        if self.compaction.is_some() {
            tools.register(CompactTool);
        }
        let observer: Arc<dyn AgentObserver> =
            self.observer.unwrap_or_else(|| Arc::new(NoopObserver));
        if let Some(registry) = &self.mcp {
            let observer = Arc::clone(&observer);
            registry.set_listener(Arc::new(move |server: &str, tools: usize| {
                observer.on_event(AgentEvent::McpConnected { server, tools });
            }));
        }
        if let Some(registry) = &self.workflow {
            let observer = Arc::clone(&observer);
            registry.set_listener(Arc::new(move |event: &WorkflowEvent| {
                observer.on_event(AgentEvent::WorkflowReported {
                    run_id: &event.run_id,
                    kind: &event.kind,
                    message: &event.message,
                });
            }));
        }
        let client = self.client.expect("AgentBuilder requires a client");
        let compactor = self.compaction.map(|config| {
            let observer = Arc::clone(&observer);
            Arc::new(
                Compactor::new(Arc::clone(&client), workspace.clone(), config).with_listener(
                    Arc::new(move |kind: &str, transcript: Option<&Path>| {
                        observer.on_event(AgentEvent::Compacted { kind, transcript });
                    }),
                ),
            )
        });

        let memory = self.memory.map(|config| {
            let recall = self
                .memory_recall
                .unwrap_or_else(|| Arc::new(CatalogRecall::new(Arc::clone(&client))));
            Arc::new(MemoryManager::new(
                Arc::clone(&client),
                workspace.clone(),
                config,
                recall,
            ))
        });

        Agent {
            client,
            tools: Arc::new(tools),
            compactor,
            memory,
            ctx: ToolContext::new(workspace),
            hooks,
            observer,
            background: self.background,
            cron: self.cron,
            team: self.team,
            mcp: self.mcp,
            workflow: self.workflow,
            recovery: self.recovery,
            interactive,
            system_prompt,
            max_tokens: self.max_tokens.unwrap_or(8192),
            max_steps: self.max_steps.unwrap_or(64),
            todo_reminder_after: self.todo_reminder_after.unwrap_or(3),
        }
    }
}

/// A sensible default system prompt, mirroring the reference harness.
pub fn default_system_prompt() -> String {
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let environment = if cfg!(windows) {
        "Windows: the bash tool runs through cmd.exe; use cmd.exe syntax, not Unix Bash or PowerShell syntax, and prefer the dedicated file tools."
    } else {
        "Unix-like: the bash tool runs the system shell."
    };
    format!(
        "You are a coding agent at {cwd}. Environment: {environment} Before starting any multi-step task, use todo_write to plan your steps, then update status as you go."
    )
}

/// Fold completed background notifications into the conversation.
///
/// They extend the trailing user message when there is one (the tool-result
/// round the loop just pushed), so a notification lands next to the results
/// the model is already reading.
fn inject_notifications(history: &mut Vec<Message>, notifications: Vec<String>) {
    let blocks: Vec<ContentBlock> = notifications.into_iter().map(ContentBlock::text).collect();
    match history.last_mut() {
        Some(message) if message.role == Role::User => message.content.extend(blocks),
        _ => history.push(Message::user_blocks(blocks)),
    }
}

/// The most recent user text, kept verbatim in any compaction summary so the
/// current ask survives the rewrite.
fn active_request(history: &[Message]) -> String {
    history
        .iter()
        .rev()
        .filter(|message| message.role == Role::User)
        .map(Message::text)
        .find(|text| !text.trim().is_empty())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::llm::{CompletionRequest, CompletionResponse};
    use crate::tools::Tool;

    /// Replays a scripted list of responses.
    struct ScriptedClient {
        responses: Mutex<VecDeque<CompletionResponse>>,
    }

    impl ScriptedClient {
        fn new(responses: Vec<CompletionResponse>) -> Self {
            ScriptedClient {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn complete(&self, _request: CompletionRequest<'_>) -> Result<CompletionResponse> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::Config("no scripted response left".into()))
        }
    }

    /// A tool that echoes its `value` argument back.
    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo a value."
        }
        fn input_schema(&self) -> Value {
            json!({ "type": "object", "properties": { "value": { "type": "string" } } })
        }
        async fn call(&self, _ctx: &ToolContext, input: Value) -> Result<String> {
            Ok(input
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string())
        }
    }

    fn echo_registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        registry
    }

    fn response(content: Vec<ContentBlock>, stop_reason: &str) -> CompletionResponse {
        CompletionResponse {
            content,
            stop_reason: Some(stop_reason.to_string()),
            model: None,
            usage: None,
        }
    }

    fn tool_use(id: &str, name: &str, input: Value) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    #[tokio::test]
    async fn loop_dispatches_tools_then_stops() {
        let client = ScriptedClient::new(vec![
            response(
                vec![tool_use("call_1", "echo", json!({ "value": "hello" }))],
                "tool_use",
            ),
            response(vec![ContentBlock::text("done")], "end_turn"),
        ]);

        let agent = Agent::builder()
            .client(client)
            .tools(echo_registry())
            .workspace(std::env::temp_dir())
            .build();

        let mut history = vec![Message::user_text("say hello")];
        agent.run_turn(&mut history).await.unwrap();

        // user, assistant(tool_use), user(tool_result), assistant(text)
        assert_eq!(history.len(), 4);
        match &history[2].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert_eq!(content, "hello");
                assert!(!is_error);
            }
            other => panic!("expected tool result, got {other:?}"),
        }
        assert_eq!(history[3].text(), "done");
    }

    #[tokio::test]
    async fn deny_list_blocks_bash() {
        let client = ScriptedClient::new(vec![
            response(
                vec![tool_use("call_1", "bash", json!({ "command": "rm -rf /" }))],
                "tool_use",
            ),
            response(vec![ContentBlock::text("ok")], "end_turn"),
        ]);

        let agent = Agent::builder()
            .client(client)
            .tools(ToolRegistry::with_defaults())
            .workspace(std::env::temp_dir())
            .build();

        let mut history = vec![Message::user_text("clean up")];
        agent.run_turn(&mut history).await.unwrap();

        match &history[2].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(content.contains("denied"), "unexpected output: {content}");
                assert!(*is_error);
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    /// A `PreToolUse` hook that blocks every `bash` call.
    struct BlockBash;

    impl Hook for BlockBash {
        fn pre_tool_use(&self, call: &ToolCall) -> Option<String> {
            (call.name == "bash").then(|| "bash is disabled by hook".to_string())
        }
    }

    #[tokio::test]
    async fn pre_tool_use_hook_blocks_call() {
        let client = ScriptedClient::new(vec![
            response(
                vec![tool_use("call_1", "bash", json!({ "command": "echo hi" }))],
                "tool_use",
            ),
            response(vec![ContentBlock::text("ok")], "end_turn"),
        ]);

        let agent = Agent::builder()
            .client(client)
            .tools(ToolRegistry::with_defaults())
            .workspace(std::env::temp_dir())
            .hook(BlockBash)
            .build();

        let mut history = vec![Message::user_text("say hi")];
        agent.run_turn(&mut history).await.unwrap();

        match &history[2].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert_eq!(content, "bash is disabled by hook");
                assert!(*is_error);
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    /// A `Stop` hook that forces exactly one extra turn.
    struct ContinueOnce {
        fired: Mutex<bool>,
    }

    impl Hook for ContinueOnce {
        fn stop(&self, _history: &[Message]) -> Option<String> {
            let mut fired = self.fired.lock().unwrap();
            if *fired {
                None
            } else {
                *fired = true;
                Some("keep going".to_string())
            }
        }
    }

    #[tokio::test]
    async fn stop_hook_forces_continuation() {
        let client = ScriptedClient::new(vec![
            response(vec![ContentBlock::text("first")], "end_turn"),
            response(vec![ContentBlock::text("second")], "end_turn"),
        ]);

        let agent = Agent::builder()
            .client(client)
            .tools(echo_registry())
            .workspace(std::env::temp_dir())
            .hook(ContinueOnce {
                fired: Mutex::new(false),
            })
            .build();

        let mut history = vec![Message::user_text("go")];
        agent.run_turn(&mut history).await.unwrap();

        // user, assistant("first"), user("keep going"), assistant("second")
        assert_eq!(history.len(), 4);
        assert_eq!(history[2].text(), "keep going");
        assert_eq!(history[3].text(), "second");
    }

    #[tokio::test]
    async fn todo_reminder_injected_after_three_rounds() {
        let client = ScriptedClient::new(vec![
            response(
                vec![tool_use("c1", "echo", json!({ "value": "a" }))],
                "tool_use",
            ),
            response(
                vec![tool_use("c2", "echo", json!({ "value": "b" }))],
                "tool_use",
            ),
            response(
                vec![tool_use("c3", "echo", json!({ "value": "c" }))],
                "tool_use",
            ),
            response(vec![ContentBlock::text("done")], "end_turn"),
        ]);

        let agent = Agent::builder()
            .client(client)
            .tools(echo_registry())
            .workspace(std::env::temp_dir())
            .build();

        let mut history = vec![Message::user_text("work")];
        agent.run_turn(&mut history).await.unwrap();

        // After the third round without a todo update the reminder rides along
        // with the tool results.
        let has_reminder = history[6]
            .content
            .iter()
            .any(|block| block.as_text() == Some(TODO_REMINDER));
        assert!(has_reminder, "expected reminder, got {:#?}", history[6]);
        assert_eq!(history[7].text(), "done");
    }

    /// A `UserPromptSubmit` hook that injects context.
    struct InjectContext;

    impl Hook for InjectContext {
        fn user_prompt_submit(&self, _prompt: &str) -> Option<String> {
            Some("workspace: /tmp".to_string())
        }
    }

    #[test]
    fn prepare_prompt_injects_context() {
        let agent = Agent::builder()
            .client(ScriptedClient::new(vec![]))
            .workspace(std::env::temp_dir())
            .hook(InjectContext)
            .build();

        assert_eq!(agent.prepare_prompt("hi"), "workspace: /tmp\n\nhi");
    }

    #[test]
    fn tool_context_blocks_escapes() {
        let ctx = ToolContext::new(std::env::temp_dir());
        assert!(ctx.resolve("../outside").is_err());
        assert!(ctx.resolve("nested/file.txt").is_ok());
    }

    /// Records the system prompt and tool names of every request.
    struct CapturingClient {
        systems: Mutex<Vec<String>>,
        tool_names: Mutex<Vec<Vec<String>>>,
    }

    impl CapturingClient {
        fn new() -> Self {
            CapturingClient {
                systems: Mutex::new(Vec::new()),
                tool_names: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LlmClient for CapturingClient {
        async fn complete(&self, request: CompletionRequest<'_>) -> Result<CompletionResponse> {
            self.systems
                .lock()
                .unwrap()
                .push(request.system.to_string());
            self.tool_names
                .lock()
                .unwrap()
                .push(request.tools.iter().map(|tool| tool.name.clone()).collect());
            Ok(response(vec![ContentBlock::text("done")], "end_turn"))
        }
    }

    #[tokio::test]
    async fn skills_are_wired_into_prompt_and_tools() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("skills").join("demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill.\n---\n\nDo the demo.",
        )
        .unwrap();

        let capturing = Arc::new(CapturingClient::new());
        let client: Arc<dyn LlmClient> = capturing.clone();
        let agent = Agent::builder()
            .shared_client(client)
            .workspace(dir.path())
            .skills(SkillLoader::new(dir.path().join("skills")))
            .build();

        let mut history = vec![Message::user_text("hi")];
        agent.run_turn(&mut history).await.unwrap();

        let systems = capturing.systems.lock().unwrap().clone();
        assert!(systems[0].contains("- demo: Demo skill."), "{}", systems[0]);
        let tools = capturing.tool_names.lock().unwrap().clone();
        assert!(
            tools[0].iter().any(|name| name == "load_skill"),
            "{:?}",
            tools[0]
        );
    }

    #[tokio::test]
    async fn compact_tool_summarises_the_history() {
        let dir = tempfile::tempdir().unwrap();
        let client = ScriptedClient::new(vec![
            response(vec![tool_use("call_1", "compact", json!({}))], "tool_use"),
            response(vec![ContentBlock::text("the summary")], "end_turn"),
            response(vec![ContentBlock::text("all done")], "end_turn"),
        ]);

        let agent = Agent::builder()
            .client(client)
            .workspace(dir.path())
            .compaction(CompactConfig::default())
            .build();

        let mut history = vec![Message::user_text("long task")];
        agent.run_turn(&mut history).await.unwrap();

        assert_eq!(history.len(), 2);
        assert!(
            history[0].text().starts_with("[Compacted]"),
            "{}",
            history[0].text()
        );
        assert!(history[0].text().contains("long task"));
        assert_eq!(history[1].text(), "all done");
    }

    /// Fails the first call with `prompt_too_long`, then answers.
    struct OverflowOnceClient {
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl LlmClient for OverflowOnceClient {
        async fn complete(&self, _request: CompletionRequest<'_>) -> Result<CompletionResponse> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            match *calls {
                1 => Err(Error::Api {
                    status: 400,
                    message: "prompt_too_long".to_string(),
                }),
                2 => Ok(response(
                    vec![ContentBlock::text("summary of the past")],
                    "end_turn",
                )),
                _ => Ok(response(
                    vec![ContentBlock::text("final answer")],
                    "end_turn",
                )),
            }
        }
    }

    #[tokio::test]
    async fn reactive_compaction_recovers_from_an_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let agent = Agent::builder()
            .client(OverflowOnceClient {
                calls: Mutex::new(0),
            })
            .workspace(dir.path())
            .compaction(CompactConfig::default())
            .build();

        let mut history = vec![Message::user_text("explain the repo")];
        agent.run_turn(&mut history).await.unwrap();

        assert_eq!(history.len(), 2);
        assert!(
            history[0].text().starts_with("[Reactive compact]"),
            "{}",
            history[0].text()
        );
        assert!(history[0].text().contains("summary of the past"));
        assert_eq!(history[1].text(), "final answer");
    }

    #[tokio::test]
    async fn a_background_bash_call_returns_a_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let client = ScriptedClient::new(vec![
            response(
                vec![tool_use(
                    "call_1",
                    "bash",
                    json!({ "command": "echo inline-marker", "run_in_background": true }),
                )],
                "tool_use",
            ),
            response(vec![ContentBlock::text("carrying on")], "end_turn"),
        ]);
        let agent = Agent::builder()
            .client(client)
            .tools(ToolRegistry::with_defaults())
            .workspace(dir.path())
            .background(BackgroundManager::new())
            .build();

        let mut history = vec![Message::user_text("start it")];
        agent.run_turn(&mut history).await.unwrap();

        let results = history
            .iter()
            .filter(|message| message.role == Role::User)
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            results
                .iter()
                .any(|text| text.contains("Background task bg_0001 started")),
            "{results:?}"
        );
        // The command was dispatched, not executed inline.
        assert!(!results.iter().any(|text| text.contains("inline-marker")));
        wait_for_background(&agent);
        assert_eq!(agent.background().unwrap().collect().len(), 1);
    }

    /// Block until the agent's background manager has a queued result.
    fn wait_for_background(agent: &Agent) {
        let manager = agent.background().expect("background enabled");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while manager.ready() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "background task did not finish in time"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn a_finished_background_result_is_injected_on_a_later_turn() {
        let dir = tempfile::tempdir().unwrap();
        let client = ScriptedClient::new(vec![
            response(
                vec![tool_use(
                    "call_1",
                    "bash",
                    json!({ "command": "echo later-visible", "run_in_background": true }),
                )],
                "tool_use",
            ),
            response(vec![ContentBlock::text("started")], "end_turn"),
            response(vec![ContentBlock::text("noted")], "end_turn"),
        ]);
        let agent = Agent::builder()
            .client(client)
            .tools(ToolRegistry::with_defaults())
            .workspace(dir.path())
            .background(BackgroundManager::new())
            .build();

        let mut history = vec![Message::user_text("kick it off")];
        agent.run_turn(&mut history).await.unwrap();
        wait_for_background(&agent);

        history.push(Message::user_text("any update?"));
        agent.run_turn(&mut history).await.unwrap();

        let injected = history
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(ContentBlock::as_text)
            .find(|text| text.contains("<task_notification>"))
            .expect("a task_notification should have been injected");
        assert!(
            injected.contains("<status>completed</status>"),
            "{injected}"
        );
        assert!(injected.contains("later-visible"), "{injected}");
    }

    #[tokio::test]
    async fn a_due_cron_job_becomes_a_scheduled_user_message() {
        let dir = tempfile::tempdir().unwrap();
        let cron = CronScheduler::new(dir.path());
        let job = cron
            .schedule("* * * * *", "run tests", true, false)
            .unwrap();
        cron.poll(CronScheduler::now()).unwrap();
        assert!(cron.has_due());

        let client = ScriptedClient::new(vec![response(
            vec![ContentBlock::text("on it")],
            "end_turn",
        )]);
        let agent = Agent::builder()
            .client(client)
            .tools(echo_registry())
            .workspace(dir.path())
            .cron(cron.clone())
            .build();

        let mut history = Vec::new();
        agent.run_turn(&mut history).await.unwrap();

        assert_eq!(history.len(), 2);
        assert!(
            history[0].text().contains("[Scheduled] run tests"),
            "{}",
            history[0].text()
        );
        // Acknowledged: the recurring job stays and waits for the next match.
        assert!(!cron.has_due());
        assert_eq!(cron.jobs().len(), 1);
        assert_eq!(cron.jobs()[0].id, job.id);
    }

    #[tokio::test]
    async fn a_one_shot_cron_job_is_removed_once_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let cron = CronScheduler::new(dir.path());
        cron.schedule("* * * * *", "once only", false, false)
            .unwrap();
        cron.poll(CronScheduler::now()).unwrap();

        let client =
            ScriptedClient::new(vec![response(vec![ContentBlock::text("ok")], "end_turn")]);
        let agent = Agent::builder()
            .client(client)
            .tools(echo_registry())
            .workspace(dir.path())
            .cron(cron.clone())
            .build();

        let mut history = Vec::new();
        agent.run_turn(&mut history).await.unwrap();

        assert!(cron.jobs().is_empty());
        assert!(!cron.has_due());
    }

    #[tokio::test]
    async fn a_failed_call_rolls_back_the_scheduled_message() {
        let dir = tempfile::tempdir().unwrap();
        let cron = CronScheduler::new(dir.path());
        cron.schedule("* * * * *", "retry me", true, false).unwrap();
        cron.poll(CronScheduler::now()).unwrap();

        let client = ScriptedClient::new(vec![]);
        let agent = Agent::builder()
            .client(client)
            .tools(echo_registry())
            .workspace(dir.path())
            .cron(cron.clone())
            .build();

        let mut history = vec![Message::user_text("keep me")];
        let error = agent.run_turn(&mut history).await.unwrap_err();
        assert!(matches!(error, Error::Config(_)), "{error:?}");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text(), "keep me");
        // The job is queued again for the next turn.
        assert!(cron.has_due());
    }

    #[tokio::test]
    async fn teammate_events_are_injected_into_the_lead_turn() {
        let dir = tempfile::tempdir().unwrap();
        let team = Team::new(dir.path(), Arc::new(ScriptedClient::new(vec![])));
        team.bus()
            .send(&crate::team::TeamMessage::new(
                "bob",
                "lead",
                "auth done",
                "result",
            ))
            .unwrap();
        assert!(team.bus().peek("lead").unwrap());

        let client = ScriptedClient::new(vec![response(
            vec![ContentBlock::text("noted")],
            "end_turn",
        )]);
        let agent = Agent::builder()
            .client(client)
            .tools(echo_registry())
            .workspace(dir.path())
            .team(team.clone())
            .build();

        let mut history = vec![Message::user_text("status?")];
        agent.run_turn(&mut history).await.unwrap();

        let injected = history
            .iter()
            .map(Message::text)
            .find(|text| text.contains("[Team events]"))
            .expect("team events should have been injected");
        assert!(injected.contains("[result] bob: auth done"), "{injected}");
        // The Lead inbox is drained once the events are delivered.
        assert!(!team.bus().peek("lead").unwrap());
    }

    #[tokio::test]
    async fn a_connected_mcp_tool_is_dispatched_through_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = McpRegistry::new();
        let client = ScriptedClient::new(vec![
            response(
                vec![tool_use("call_1", "connect_mcp", json!({ "name": "docs" }))],
                "tool_use",
            ),
            response(
                vec![tool_use(
                    "call_2",
                    "mcp__docs__search",
                    json!({ "query": "hooks" }),
                )],
                "tool_use",
            ),
            response(vec![ContentBlock::text("done")], "end_turn"),
        ]);
        let agent = Agent::builder()
            .client(client)
            .tools(echo_registry())
            .workspace(dir.path())
            .mcp(mcp.clone())
            .build();

        let mut history = vec![Message::user_text("search the docs")];
        agent.run_turn(&mut history).await.unwrap();

        // The connect tool ran, and the discovered tool resolved two rounds on.
        assert!(mcp.is_connected("docs"));
        let (output, is_error) = history
            .iter()
            .flat_map(|message| message.content.iter())
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    content, is_error, ..
                } if content.contains("[docs]") => Some((content.clone(), *is_error)),
                _ => None,
            })
            .expect("the discovered MCP tool result should be in history");
        assert!(!is_error, "{output}");
        assert_eq!(output, "[docs] Found 3 results for 'hooks'");
    }

    #[tokio::test]
    async fn the_host_policy_gates_unapproved_mcp_tools() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = McpRegistry::new();
        let client = ScriptedClient::new(vec![
            response(
                vec![tool_use(
                    "call_1",
                    "connect_mcp",
                    json!({ "name": "deploy" }),
                )],
                "tool_use",
            ),
            response(
                vec![tool_use(
                    "call_2",
                    "mcp__deploy__trigger",
                    json!({ "service": "web" }),
                )],
                "tool_use",
            ),
            response(vec![ContentBlock::text("ok")], "end_turn"),
        ]);
        let agent = Agent::builder()
            .client(client)
            .tools(echo_registry())
            .workspace(dir.path())
            .mcp(mcp.clone())
            .approver(crate::permission::DenyApprover)
            .build();

        let mut history = vec![Message::user_text("deploy web")];
        agent.run_turn(&mut history).await.unwrap();

        let blocked = history
            .iter()
            .flat_map(|message| message.content.iter())
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    content, is_error, ..
                } if *is_error && content.contains("Permission denied") => Some(content.clone()),
                _ => None,
            })
            .expect("the unapproved MCP tool should have been blocked");
        assert!(blocked.contains("mcp__deploy__trigger"), "{blocked}");
    }

    /// Replays a script of successes *and* failures, recording the model asked for.
    struct FlakyClient {
        script: Mutex<VecDeque<Result<CompletionResponse>>>,
        models: Mutex<Vec<Option<String>>>,
    }

    impl FlakyClient {
        fn new(script: Vec<Result<CompletionResponse>>) -> Self {
            FlakyClient {
                script: Mutex::new(script.into()),
                models: Mutex::new(Vec::new()),
            }
        }

        fn models(&self) -> Vec<Option<String>> {
            self.models.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmClient for FlakyClient {
        async fn complete(&self, request: CompletionRequest<'_>) -> Result<CompletionResponse> {
            self.models
                .lock()
                .unwrap()
                .push(request.model.map(str::to_string));
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(Error::Config("no scripted response left".into())))
        }
    }

    /// Records only the s15 recovery events.
    struct RecoveryObserver {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl AgentObserver for RecoveryObserver {
        fn on_event(&self, event: AgentEvent<'_>) {
            let label = match event {
                AgentEvent::ModelRecovered { kind, attempt, .. } => {
                    format!("recovered {kind} #{attempt}")
                }
                AgentEvent::ModelFellBack { model } => format!("fellback {model}"),
                AgentEvent::MaxTokensEscalated { max_tokens } => format!("escalated {max_tokens}"),
                AgentEvent::ContinuationRequested { count } => format!("continuation #{count}"),
                _ => return,
            };
            self.events.lock().unwrap().push(label);
        }
    }

    fn fast_recovery() -> RecoveryConfig {
        RecoveryConfig {
            base_delay_ms: 1,
            max_delay_ms: 2,
            ..RecoveryConfig::default()
        }
    }

    fn recovery_observer() -> (RecoveryObserver, Arc<Mutex<Vec<String>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            RecoveryObserver {
                events: Arc::clone(&events),
            },
            events,
        )
    }

    #[tokio::test]
    async fn a_rate_limited_call_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let (observer, events) = recovery_observer();
        let client = FlakyClient::new(vec![
            Err(Error::Api {
                status: 429,
                message: "slow down".into(),
            }),
            Ok(response(vec![ContentBlock::text("recovered")], "end_turn")),
        ]);
        let agent = Agent::builder()
            .client(client)
            .tools(echo_registry())
            .workspace(dir.path())
            .recovery(fast_recovery())
            .observer(observer)
            .build();

        let mut history = vec![Message::user_text("go")];
        agent.run_turn(&mut history).await.unwrap();

        assert_eq!(history.last().unwrap().text(), "recovered");
        let events = events.lock().unwrap();
        assert!(
            events.iter().any(|event| event == "recovered 429 #1"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn repeated_overloads_switch_to_the_fallback_model() {
        let dir = tempfile::tempdir().unwrap();
        let (observer, events) = recovery_observer();
        let overload = || Error::Api {
            status: 529,
            message: "overloaded".into(),
        };
        let client = Arc::new(FlakyClient::new(vec![
            Err(overload()),
            Err(overload()),
            Ok(response(
                vec![ContentBlock::text("from backup")],
                "end_turn",
            )),
        ]));
        let config = RecoveryConfig {
            base_delay_ms: 1,
            max_delay_ms: 2,
            fallback_model: Some("backup-model".to_string()),
            ..RecoveryConfig::default()
        };
        let agent = Agent::builder()
            .shared_client(client.clone())
            .tools(echo_registry())
            .workspace(dir.path())
            .recovery(config)
            .observer(observer)
            .build();

        let mut history = vec![Message::user_text("go")];
        agent.run_turn(&mut history).await.unwrap();

        assert_eq!(history.last().unwrap().text(), "from backup");
        let models = client.models();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0], None);
        assert_eq!(models[2].as_deref(), Some("backup-model"));
        let events = events.lock().unwrap();
        assert!(
            events.iter().any(|event| event == "fellback backup-model"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_truncated_response_is_retried_with_a_bigger_budget() {
        let dir = tempfile::tempdir().unwrap();
        let (observer, events) = recovery_observer();
        let client = ScriptedClient::new(vec![
            response(vec![ContentBlock::text("half")], "max_tokens"),
            response(vec![ContentBlock::text("whole answer")], "end_turn"),
        ]);
        let agent = Agent::builder()
            .client(client)
            .tools(echo_registry())
            .workspace(dir.path())
            .recovery(fast_recovery())
            .observer(observer)
            .build();

        let mut history = vec![Message::user_text("go")];
        agent.run_turn(&mut history).await.unwrap();

        // The truncated text is discarded, not appended.
        assert_eq!(history.last().unwrap().text(), "whole answer");
        assert!(!history.iter().any(|message| message.text() == "half"));
        let events = events.lock().unwrap();
        assert!(
            events.iter().any(|event| event == "escalated 16000"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_truncated_turn_with_nothing_to_run_is_continued() {
        let dir = tempfile::tempdir().unwrap();
        let (observer, events) = recovery_observer();
        let client = ScriptedClient::new(vec![
            response(vec![ContentBlock::text("half")], "max_tokens"),
            response(vec![ContentBlock::text("rest")], "end_turn"),
        ]);
        let config = RecoveryConfig {
            base_delay_ms: 1,
            max_delay_ms: 2,
            escalated_max_tokens: 8192, // no headroom: escalation is skipped
            ..RecoveryConfig::default()
        };
        let agent = Agent::builder()
            .client(client)
            .tools(echo_registry())
            .workspace(dir.path())
            .recovery(config)
            .observer(observer)
            .build();

        let mut history = vec![Message::user_text("go")];
        agent.run_turn(&mut history).await.unwrap();

        let texts: Vec<String> = history.iter().map(Message::text).collect();
        assert!(texts.iter().any(|text| text == "half"), "{texts:?}");
        assert!(
            texts.iter().any(|text| text == CONTINUATION_PROMPT),
            "{texts:?}"
        );
        assert_eq!(history.last().unwrap().text(), "rest");
        let events = events.lock().unwrap();
        assert!(
            events.iter().any(|event| event == "continuation #1"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_non_interactive_turn_denies_instead_of_asking() {
        let dir = tempfile::tempdir().unwrap();
        let client = ScriptedClient::new(vec![
            response(
                vec![tool_use("c1", "bash", json!({ "command": "rm -rf build" }))],
                "tool_use",
            ),
            response(vec![ContentBlock::text("understood")], "end_turn"),
        ]);
        let agent = Agent::builder()
            .client(client)
            .tools(ToolRegistry::with_defaults())
            .workspace(dir.path())
            .interactive(false)
            .build();

        let mut history = vec![Message::user_text("clean up")];
        agent.run_turn(&mut history).await.unwrap();

        let blocked = history
            .iter()
            .flat_map(|message| message.content.iter())
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    content, is_error, ..
                } if *is_error && content.contains("asynchronous turn") => Some(content.clone()),
                _ => None,
            })
            .expect("a destructive command should be denied without a prompt");
        assert!(blocked.contains("Permission denied"), "{blocked}");
    }

    #[tokio::test]
    async fn the_workflow_tool_is_registered_and_reported() {
        struct RecordingObserver(Arc<Mutex<Vec<String>>>);

        impl AgentObserver for RecordingObserver {
            fn on_event(&self, event: AgentEvent<'_>) {
                if let AgentEvent::WorkflowReported { kind, .. } = event {
                    self.0.lock().unwrap().push(kind.to_string());
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let agent = Agent::builder()
            .client(ScriptedClient::new(Vec::new()))
            .tools(ToolRegistry::with_defaults())
            .workspace(dir.path())
            .workflow(crate::workflow::default_registry().unwrap())
            .observer(RecordingObserver(Arc::clone(&events)))
            .build();

        let names: Vec<String> = agent
            .tools
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        assert!(names.contains(&crate::workflow::WORKFLOW_TOOL_NAME.to_string()));

        let tool = agent
            .tools
            .get(crate::workflow::WORKFLOW_TOOL_NAME)
            .expect("the Workflow tool is registered");
        let output = tool
            .call(
                agent.context(),
                json!({ "name": "review-changes", "args": { "changes": "let x = 1;" } }),
            )
            .await
            .unwrap();
        assert!(output.contains("local_workflow_"));

        let seen = events.lock().unwrap().clone();
        assert!(seen.contains(&"async_launched".to_string()), "{seen:?}");
        assert!(seen.contains(&"task_notification".to_string()), "{seen:?}");
        assert!(seen.contains(&"workflow_phase".to_string()), "{seen:?}");
    }
}
