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
use crate::error::{Error, Result};
use crate::hooks::{Hook, Hooks};
use crate::llm::{CompletionRequest, LlmClient};
use crate::memory::{CatalogRecall, MemoryConfig, MemoryManager, MemoryRecall};
use crate::message::{ContentBlock, Message, Role, ToolCall};
use crate::permission::{Approver, AutoApprover, PermissionHook, PermissionPolicy};
use crate::skills::SkillLoader;
use crate::tools::{BashTool, LoadSkillTool, ToolContext, ToolRegistry, TODO_TOOL_NAME};

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
        let tools = self.tools.definitions();
        let mut rounds_since_todo = 0usize;
        let active_request = active_request(history);
        let mut reactive_retries = 0usize;

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

            let response = match self
                .client
                .complete(CompletionRequest {
                    system: &system_prompt,
                    messages: history,
                    tools: &tools,
                    max_tokens: self.max_tokens,
                })
                .await
            {
                Ok(response) => {
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
                    _ => return Err(error),
                },
            };

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

    async fn dispatch(&self, call: &ToolCall) -> (String, bool) {
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
        hooks.register(PermissionHook::new(
            self.policy
                .unwrap_or_else(|| PermissionPolicy::new(workspace.clone())),
            self.approver.unwrap_or_else(|| Arc::new(AutoApprover)),
        ));
        for hook in self.hooks {
            hooks.register_arc(hook);
        }

        let mut tools = self.tools.unwrap_or_else(ToolRegistry::with_defaults);
        // s11: only advertise `run_in_background` when a manager can honour it.
        if self.background.is_some() {
            tools.register(BashTool::default().advertising_background());
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
}
