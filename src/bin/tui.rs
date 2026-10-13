//! A ratatui TUI over the agent loop (experimental, `cargo run --bin tui`).
//!
//! The agent runs on its own thread with a current-thread Tokio runtime, so a
//! blocking approval handshake never stalls the UI. The two sides talk over
//! channels: commands go down, events come up, and approvals rendezvous on a
//! `Mutex`/`Condvar` gate.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::Result;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};

use rust_agent::agent::{AgentEvent, AgentObserver};
use rust_agent::permission::Approver;
use rust_agent::subagent::{subagent_system_prompt, TaskTool};
use rust_agent::ui::{self, ApprovalView, Entry, EntryKind, StatusInfo, Theme};
use rust_agent::{
    default_registry, default_system_prompt, parse_goal_command, Agent, AnthropicAgentRunner,
    AnthropicClient, BackgroundManager, CompactConfig, CompletionRequest, CompletionResponse,
    Config, ContentBlock, CronScheduler, GoalCommand, GoalController, LlmClient, McpRegistry,
    MemoryConfig, Message, PromptGoalEvaluator, RecoveryConfig, SkillLoader, Team, ToolCall,
    ToolRegistry, BACKGROUND_PROMPT, CRON_PROMPT, GOAL_PROMPT, MCP_PROMPT, TASKS_PROMPT,
    TEAM_PROMPT, WORKFLOW_PROMPT,
};

/// What the UI asks the agent thread to do.
enum Command {
    Prompt(String),
    Goal(GoalCommand),
    Reset,
    Quit,
}

/// What the agent thread reports back to the UI.
enum UiEvent {
    AssistantText(String),
    ToolCall {
        name: String,
        input: String,
    },
    ToolResult {
        name: String,
        output: String,
        is_error: bool,
    },
    Blocked {
        name: String,
        reason: String,
    },
    Compacted {
        kind: String,
        transcript: Option<String>,
    },
    MemoryRecalled {
        count: usize,
    },
    MemoryStored {
        stored: usize,
        consolidated: usize,
    },
    /// A lifecycle line from another subsystem (s11-s17).
    Notice(String),
    Approval {
        name: String,
        input: String,
        reason: String,
    },
    Error(String),
    /// The agent thread picked up a turn (a prompt, or idle work).
    TurnStarted,
    TurnDone,
}

/// Rendezvous for the blocking approval handshake.
#[derive(Default)]
struct ApprovalGate {
    decision: Mutex<Option<bool>>,
    ready: Condvar,
    /// Tool names the user waved through for the rest of the session.
    always: Mutex<HashSet<String>>,
}

impl ApprovalGate {
    /// Block the agent thread until the UI answers.
    fn wait(&self) -> bool {
        let mut guard = self.decision.lock().unwrap();
        while guard.is_none() {
            guard = self.ready.wait(guard).unwrap();
        }
        guard.take().unwrap_or(false)
    }

    /// Answer a pending request from the UI thread.
    fn answer(&self, allow: bool) {
        *self.decision.lock().unwrap() = Some(allow);
        self.ready.notify_all();
    }

    /// Stop asking about `name` for the rest of the session.
    fn allow_always(&self, name: &str) {
        self.always.lock().unwrap().insert(name.to_string());
    }

    /// True when the user already said "always" for this tool.
    fn is_always(&self, name: &str) -> bool {
        self.always.lock().unwrap().contains(name)
    }
}

/// Forwards agent events to the UI, optionally prefixed (for subagents).
struct TuiObserver {
    tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    prefix: &'static str,
    show_text: bool,
}

impl TuiObserver {
    fn new(
        tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
        prefix: &'static str,
        show_text: bool,
    ) -> Self {
        TuiObserver {
            tx,
            prefix,
            show_text,
        }
    }
}

impl AgentObserver for TuiObserver {
    fn on_event(&self, event: AgentEvent<'_>) {
        let event = match event {
            AgentEvent::AssistantText(text) if self.show_text => {
                UiEvent::AssistantText(text.to_string())
            }
            AgentEvent::AssistantText(_) | AgentEvent::Step(_) => return,
            AgentEvent::ToolCall { name, input } => UiEvent::ToolCall {
                name: format!("{}{name}", self.prefix),
                input: compact(input),
            },
            AgentEvent::ToolResult {
                name,
                output,
                is_error,
            } => UiEvent::ToolResult {
                name: format!("{}{name}", self.prefix),
                output: preview(output),
                is_error,
            },
            AgentEvent::Blocked { name, reason } => UiEvent::Blocked {
                name: format!("{}{name}", self.prefix),
                reason: reason.to_string(),
            },
            AgentEvent::Compacted { kind, transcript } => UiEvent::Compacted {
                kind: kind.to_string(),
                transcript: transcript.map(|path| path.display().to_string()),
            },
            AgentEvent::MemoryRecalled { count } => UiEvent::MemoryRecalled { count },
            AgentEvent::MemoryStored {
                stored,
                consolidated,
            } => UiEvent::MemoryStored {
                stored,
                consolidated,
            },
            // s11-s17: the subsystems main.rs wires up all report here; the
            // TUI renders them as one-line system notices.
            AgentEvent::BackgroundStarted { task_id } => {
                UiEvent::Notice(format!("[background] {task_id} running"))
            }
            AgentEvent::BackgroundCollected { count } => {
                UiEvent::Notice(format!("[background] collected {count} result(s)"))
            }
            AgentEvent::CronDelivered { count } => {
                UiEvent::Notice(format!("[cron] delivered {count} job(s)"))
            }
            AgentEvent::TeamEventDelivered { count } => {
                UiEvent::Notice(format!("[team] {count} event(s) from teammates"))
            }
            AgentEvent::McpConnected { server, tools } => {
                UiEvent::Notice(format!("[mcp] connected {server}: {tools} tool(s)"))
            }
            AgentEvent::ModelRecovered {
                kind,
                attempt,
                delay_ms,
            } => UiEvent::Notice(format!("[{kind}] retry {attempt} after {delay_ms}ms")),
            AgentEvent::ModelFellBack { model } => {
                UiEvent::Notice(format!("[529] switching to {model}"))
            }
            AgentEvent::MaxTokensEscalated { max_tokens } => {
                UiEvent::Notice(format!("[max_tokens] retry with {max_tokens}"))
            }
            AgentEvent::ContinuationRequested { count } => UiEvent::Notice(format!(
                "[continuation {count}] asking the model to continue"
            )),
            AgentEvent::WorkflowReported {
                run_id,
                kind,
                message,
            } => UiEvent::Notice(format!("[workflow {run_id}] {kind}: {message}")),
            AgentEvent::GoalReported {
                kind,
                reason,
                iterations,
                ..
            } => UiEvent::Notice(format!(
                "[goal {kind}] {reason} (evaluations: {iterations})"
            )),
        };
        let _ = self.tx.send(event);
    }
}

/// Asks the UI for a decision over the gate.
struct TuiApprover {
    tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    gate: Arc<ApprovalGate>,
}

impl Approver for TuiApprover {
    fn approve(&self, call: &ToolCall, reason: &str) -> bool {
        // The user already waved this tool through for the session.
        if self.gate.is_always(&call.name) {
            let _ = self.tx.send(UiEvent::Notice(format!(
                "[permission] {} allowed for the session",
                call.name
            )));
            return true;
        }
        let _ = self.tx.send(UiEvent::Approval {
            name: call.name.clone(),
            input: compact(&call.input),
            reason: reason.to_string(),
        });
        self.gate.wait()
    }
}

/// One-line, length-capped preview of tool input.
fn compact(value: &serde_json::Value) -> String {
    let text = value.to_string();
    if text.chars().count() <= 160 {
        text
    } else {
        text.chars().take(160).collect::<String>() + "..."
    }
}

/// First few lines of a tool result.
fn preview(text: &str) -> String {
    let total = text.lines().count();
    let mut out = text.lines().take(8).collect::<Vec<_>>().join("\n");
    if total > 8 {
        out.push_str("\n...");
    }
    out
}

/// Wire the same agent the CLI builds, but with the TUI's observer/approver.
fn build_agent(
    client: Arc<dyn LlmClient>,
    max_tokens: u32,
    workspace: PathBuf,
    tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    gate: Arc<ApprovalGate>,
) -> Result<Agent> {
    // The subagent: fresh context, base tools only, so it cannot delegate.
    let subagent = Agent::builder()
        .shared_client(client.clone())
        .tools(ToolRegistry::base())
        .workspace(workspace.clone())
        .max_tokens(max_tokens)
        .max_steps(30)
        .system_prompt(subagent_system_prompt())
        .observer(TuiObserver::new(tx.clone(), "[sub] ", false))
        .approver(TuiApprover {
            tx: tx.clone(),
            gate: gate.clone(),
        })
        .build();

    let mut tools = ToolRegistry::with_tasks();
    tools.register(TaskTool::new(subagent));

    // s12: durable scheduled prompts, restored from `.scheduled_tasks.json`.
    let cron = CronScheduler::new(workspace.clone());
    match cron.load() {
        Ok(0) => {}
        Ok(count) => {
            let _ = tx.send(UiEvent::Notice(format!(
                "[cron] restored {count} durable job(s)"
            )));
        }
        Err(error) => {
            let _ = tx.send(UiEvent::Notice(format!(
                "[cron] could not load jobs: {error}"
            )));
        }
    }
    cron.start_ticker();

    // s13: one Lead plus persistent teammates over file mailboxes; their own
    // tool traffic streams into the same transcript, prefixed.
    let team = Team::new(workspace.clone(), client.clone()).with_observer(TuiObserver::new(
        tx.clone(),
        "[team] ",
        false,
    ));

    // s14: external tools arrive through MCP servers, gated by host policy.
    let mcp = McpRegistry::new();

    // s16: saved procedures run through the host's own model client.
    let workflow =
        default_registry()?.with_runner(Arc::new(AnthropicAgentRunner::new(client.clone())));

    // s15: retry transient failures, fall back after repeated overloads.
    let mut recovery = RecoveryConfig::default();
    if let Ok(model) = std::env::var("FALLBACK_MODEL_ID") {
        let model = model.trim();
        if !model.is_empty() {
            recovery = recovery.with_fallback_model(model.to_string());
        }
    }

    // s17: an independent, tool-free evaluator judges `/goal` conditions.
    let goal =
        GoalController::with_defaults(Arc::new(PromptGoalEvaluator::from_env(client.clone())));

    Ok(Agent::builder()
        .shared_client(client)
        .tools(tools)
        .workspace(workspace.clone())
        .skills(SkillLoader::new(workspace.join("skills")))
        .compaction(CompactConfig::default())
        .memory(MemoryConfig::default())
        .background(BackgroundManager::new())
        .cron(cron)
        .team(team)
        .mcp(mcp)
        .workflow(workflow)
        .recovery(recovery)
        .goal(goal)
        .max_tokens(max_tokens)
        .system_prompt(format!(
            "{} {} {} {} {} {} {} {} Use task for focused exploration or a \
             self-contained subtask.",
            default_system_prompt(),
            TASKS_PROMPT,
            BACKGROUND_PROMPT,
            CRON_PROMPT,
            TEAM_PROMPT,
            MCP_PROMPT,
            WORKFLOW_PROMPT,
            GOAL_PROMPT
        ))
        .observer(TuiObserver::new(tx.clone(), "", true))
        .approver(TuiApprover { tx, gate })
        .build())
}

/// Owns the agent and its conversation; processes commands serially.
fn spawn_agent_thread(
    agent: Agent,
    rx: mpsc::Receiver<Command>,
    tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
) {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");
        let mut history: Vec<Message> = Vec::new();

        loop {
            match rx.recv_timeout(IDLE_POLL) {
                Ok(Command::Prompt(text)) => {
                    history.push(Message::user_text(agent.prepare_prompt(&text)));
                    run_turn(&runtime, &agent, &mut history, &tx);
                    let _ = tx.send(UiEvent::TurnDone);
                }
                // s17: `/goal` sets, inspects, and clears the session goal.
                Ok(Command::Goal(command)) => {
                    if let Some(condition) = apply_goal(&agent, command, &tx) {
                        history.push(Message::user_text(condition));
                        run_turn(&runtime, &agent, &mut history, &tx);
                    }
                    let _ = tx.send(UiEvent::TurnDone);
                }
                Ok(Command::Reset) => history.clear(),
                Ok(Command::Quit) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            // Work that arrived while the UI was idle wakes the loop on its
            // own, exactly like the CLI: a due job, a teammate, a finished
            // background command (which a pending goal is waiting for).
            if let Some(reason) = idle_wake(&agent) {
                let _ = tx.send(UiEvent::Notice(reason.to_string()));
                run_turn(&runtime, &agent, &mut history, &tx);
                let _ = tx.send(UiEvent::TurnDone);
            }
        }
    });
}

/// How long the agent thread waits for input before checking for idle work.
const IDLE_POLL: Duration = Duration::from_millis(500);

/// Run one turn, reporting a failure instead of killing the thread.
fn run_turn(
    runtime: &tokio::runtime::Runtime,
    agent: &Agent,
    history: &mut Vec<Message>,
    tx: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
) {
    let _ = tx.send(UiEvent::TurnStarted);
    if let Err(error) = runtime.block_on(agent.run_turn(history)) {
        let _ = tx.send(UiEvent::Error(error.to_string()));
    }
}

/// Apply a `/goal` command; a new condition is returned so the caller can
/// start working on it immediately.
fn apply_goal(
    agent: &Agent,
    command: GoalCommand,
    tx: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
) -> Option<String> {
    let Some(goal) = agent.goal() else {
        let _ = tx.send(UiEvent::Error("the goal loop is not enabled".to_string()));
        return None;
    };
    match command {
        GoalCommand::Status => {
            let _ = tx.send(UiEvent::Notice(goal.status()));
            None
        }
        GoalCommand::Clear => {
            let _ = tx.send(UiEvent::Notice(goal.clear("cleared")));
            None
        }
        GoalCommand::Set(condition) => match goal.set_goal(&condition) {
            Ok(state) => {
                let _ = tx.send(UiEvent::Notice(format!(
                    "[goal] active: {}",
                    state.condition
                )));
                Some(state.condition)
            }
            Err(error) => {
                let _ = tx.send(UiEvent::Error(error.to_string()));
                None
            }
        },
    }
}

/// Why the loop should start a turn while the user is idle, if anything.
fn idle_wake(agent: &Agent) -> Option<&'static str> {
    if agent.cron().is_some_and(|cron| cron.has_due()) {
        return Some("[cron] a scheduled job is due");
    }
    if agent
        .team()
        .is_some_and(|team| team.bus().peek(rust_agent::team::LEAD).unwrap_or(false))
    {
        return Some("[team] teammate events arrived");
    }
    if agent
        .background()
        .is_some_and(|background| background.ready() > 0)
    {
        return Some("[background] results ready");
    }
    None
}

/// Everything the UI needs to draw one frame.
struct UiState {
    model: String,
    workspace: PathBuf,
    skills: usize,
    theme: Theme,
    entries: Vec<Entry>,
    input: String,
    cursor: usize,
    running: bool,
    approval: Option<ApprovalView>,
    scroll: u16,
    max_scroll: u16,
    follow: bool,
    help: bool,
    tick: usize,
}

impl UiState {
    fn new(model: String, workspace: PathBuf, skills: usize) -> Self {
        UiState {
            model,
            workspace,
            skills,
            theme: Theme::default(),
            entries: Vec::new(),
            input: String::new(),
            cursor: 0,
            running: false,
            approval: None,
            scroll: 0,
            max_scroll: 0,
            follow: true,
            help: false,
            tick: 0,
        }
    }

    /// The status row for the current frame.
    fn status(&self) -> StatusInfo {
        StatusInfo {
            model: self.model.clone(),
            tokens: 0,
            cost_usd: None,
            cwd: Some(self.workspace.display().to_string()),
            busy: self.running,
            note: Some(format!("skills: {}", self.skills)),
        }
    }

    fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
        self.follow = true;
    }

    /// Append streamed model text to the block it belongs to.
    fn push_text(&mut self, text: &str) {
        if let Some(last) = self.entries.last_mut() {
            if last.kind == EntryKind::Assistant {
                last.body.push_str(text);
                self.follow = true;
                return;
            }
        }
        self.push(Entry::assistant(text));
    }

    /// Trim the trailing newline off the in-flight prose block.
    fn settle_text(&mut self) {
        if let Some(last) = self.entries.last_mut() {
            if last.kind == EntryKind::Assistant {
                let len = last.body.trim_end().len();
                last.body.truncate(len);
            }
        }
    }

    /// Stop the spinner on the call whose result just arrived.
    fn settle_call(&mut self, name: &str) {
        let call = self.entries.iter_mut().rev().find(|entry| {
            entry.kind == EntryKind::ToolCall && entry.running && entry.title == name
        });
        if let Some(call) = call {
            call.running = false;
        }
    }

    fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::AssistantText(text) => self.push_text(&text),
            UiEvent::ToolCall { name, input } => {
                self.settle_text();
                self.push(Entry::tool_call(name, input));
            }
            UiEvent::ToolResult {
                name,
                output,
                is_error,
            } => {
                self.settle_text();
                self.settle_call(&name);
                self.push(Entry::tool_result(name, output, is_error));
            }
            UiEvent::Blocked { name, reason } => {
                self.settle_text();
                self.push(Entry::error(format!("[blocked] {name}: {reason}")));
            }
            UiEvent::Compacted { kind, transcript } => {
                let suffix = transcript.map(|p| format!(" -> {p}")).unwrap_or_default();
                self.push(Entry::notice(format!("[compact:{kind}]{suffix}")));
            }
            UiEvent::MemoryRecalled { count } => {
                self.push(Entry::notice(format!("[memory] recalled {count}")));
            }
            UiEvent::MemoryStored {
                stored,
                consolidated,
            } => {
                self.push(Entry::notice(format!(
                    "[memory] stored {stored}, consolidated to {consolidated}"
                )));
            }
            UiEvent::Notice(text) => {
                self.settle_text();
                self.push(Entry::notice(text));
            }
            UiEvent::Approval {
                name,
                input,
                reason,
            } => {
                self.settle_text();
                self.push(Entry::notice(format!("[permission] {reason}")));
                self.approval = Some(ApprovalView::new(name, input, reason));
            }
            UiEvent::Error(message) => {
                self.settle_text();
                self.push(Entry::error(message));
            }
            UiEvent::TurnStarted => self.running = true,
            UiEvent::TurnDone => {
                self.settle_text();
                let running = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|entry| entry.kind == EntryKind::ToolCall && entry.running);
                if let Some(call) = running {
                    call.running = false;
                }
                self.running = false;
            }
        }
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--snapshot") {
        return snapshot(args.iter().any(|arg| arg == "--ansi"));
    }
    let demo = args.iter().any(|arg| arg == "--demo");

    dotenvy::dotenv().ok();

    let workspace = std::env::current_dir()?;

    // `--demo` runs a scripted mock model, so the UI works without a key.
    let (model, max_tokens, client): (String, u32, Arc<dyn LlmClient>) = if demo {
        ("demo-model".to_string(), 4096, Arc::new(MockClient::new()))
    } else {
        let config = Config::from_env()?;
        let client: Arc<dyn LlmClient> = Arc::new(AnthropicClient::new(&config));
        (config.model, config.max_tokens, client)
    };

    let gate = Arc::new(ApprovalGate::default());
    let (command_tx, command_rx) = mpsc::channel::<Command>();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();

    let skills = SkillLoader::new(workspace.join("skills")).len();
    let agent = build_agent(
        client,
        max_tokens,
        workspace.clone(),
        event_tx.clone(),
        gate.clone(),
    )?;
    spawn_agent_thread(agent, command_rx, event_tx);

    let mut terminal = ratatui::init();
    let mut state = UiState::new(model, workspace, skills);
    if demo {
        state.push(Entry::notice(
            "[demo] scripted model: glob -> bash (permission gate) -> answer",
        ));
    }
    let result = run_ui(&mut terminal, &mut event_rx, &command_tx, &gate, &mut state);
    ratatui::restore();
    result
}

fn run_ui(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>,
    command_tx: &mpsc::Sender<Command>,
    gate: &Arc<ApprovalGate>,
    state: &mut UiState,
) -> Result<()> {
    loop {
        while let Ok(event) = event_rx.try_recv() {
            state.apply(event);
        }

        // Keep the spinner turning while a turn is in flight.
        if state.running {
            state.tick = state.tick.wrapping_add(1);
        }
        terminal.draw(|frame| draw(frame, state))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && handle_key(key, state, command_tx, gate) {
                    let _ = command_tx.send(Command::Quit);
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Returns `true` when the user asked to quit.
fn handle_key(
    key: KeyEvent,
    state: &mut UiState,
    command_tx: &mpsc::Sender<Command>,
    gate: &Arc<ApprovalGate>,
) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }

    // Any key dismisses the help overlay.
    if state.help {
        state.help = false;
        return false;
    }

    // While a call is pending, y/a/n (or Esc to deny) answer it.
    if state.approval.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                gate.answer(true);
                state.approval = None;
                state.push(Entry::notice("[permission] allowed"));
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                if let Some(pending) = state.approval.take() {
                    gate.allow_always(&pending.name);
                }
                gate.answer(true);
                state.push(Entry::notice("[permission] allowed for the session"));
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                gate.answer(false);
                state.approval = None;
                state.push(Entry::notice("[permission] denied"));
            }
            _ => {}
        }
        return false;
    }

    if key.code == KeyCode::Esc {
        return true;
    }

    match key.code {
        // Shift+enter types a newline; plain enter sends the prompt.
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            state.input.insert(byte_index(state, state.cursor), '\n');
            state.cursor += 1;
        }
        KeyCode::Enter => submit(state, command_tx),
        KeyCode::F(1) => state.help = true,
        // `?` opens help only on an empty prompt, so it stays typeable.
        KeyCode::Char('?') if state.input.is_empty() => state.help = true,
        KeyCode::Char(c) => {
            state.input.insert(byte_index(state, state.cursor), c);
            state.cursor += 1;
        }
        KeyCode::Backspace if state.cursor > 0 => {
            let start = byte_index(state, state.cursor - 1);
            let end = byte_index(state, state.cursor);
            state.input.replace_range(start..end, "");
            state.cursor -= 1;
        }
        KeyCode::Delete if state.cursor < state.input.chars().count() => {
            let start = byte_index(state, state.cursor);
            let end = byte_index(state, state.cursor + 1);
            state.input.replace_range(start..end, "");
        }
        KeyCode::Left => state.cursor = state.cursor.saturating_sub(1),
        KeyCode::Right => state.cursor = (state.cursor + 1).min(state.input.chars().count()),
        KeyCode::Home => state.cursor = 0,
        KeyCode::End => state.cursor = state.input.chars().count(),
        KeyCode::PageUp => scroll(state, -10),
        KeyCode::PageDown => scroll(state, 10),
        KeyCode::Up => scroll(state, -1),
        KeyCode::Down => scroll(state, 1),
        _ => {}
    }
    false
}

/// Byte offset of the `n`th character (for editing a `String` by cursor).
fn byte_index(state: &UiState, n: usize) -> usize {
    state
        .input
        .char_indices()
        .nth(n)
        .map(|(index, _)| index)
        .unwrap_or(state.input.len())
}

fn submit(state: &mut UiState, command_tx: &mpsc::Sender<Command>) {
    let text = state.input.trim().to_string();
    state.input.clear();
    state.cursor = 0;
    if text.is_empty() {
        return;
    }
    match text.as_str() {
        "/reset" => {
            let _ = command_tx.send(Command::Reset);
            state.push(Entry::notice("[reset] the conversation was cleared"));
            return;
        }
        "/help" => {
            state.help = true;
            return;
        }
        "q" | "quit" | "exit" => {
            let _ = command_tx.send(Command::Quit);
            return;
        }
        _ => {}
    }
    // s17: `/goal`, `/goal <condition>`, and `/goal clear` go to the goal
    // controller on the agent thread instead of the model.
    if let Some(command) = parse_goal_command(&text) {
        state.push(Entry::user(text));
        let _ = command_tx.send(Command::Goal(command));
        return;
    }
    state.push(Entry::user(text.clone()));
    state.running = true;
    let _ = command_tx.send(Command::Prompt(text));
}

/// Move the viewport, re-enabling follow once the bottom is on screen.
fn scroll(state: &mut UiState, delta: i32) {
    let next = (state.scroll as i32 + delta).clamp(0, state.max_scroll as i32);
    state.scroll = next as u16;
    state.follow = state.scroll >= state.max_scroll;
}

/// Whether a blank line belongs between two transcript blocks.
fn separates(previous: &Entry, next: &Entry) -> bool {
    let toolish = |entry: &Entry| {
        matches!(
            entry.kind,
            EntryKind::ToolCall | EntryKind::ToolResult | EntryKind::Error
        )
    };
    // A result hangs straight off the call it answers.
    if matches!(next.kind, EntryKind::ToolResult | EntryKind::Error) && toolish(previous) {
        return false;
    }
    // Runs of system notices stay together.
    !(previous.kind == EntryKind::Notice && next.kind == EntryKind::Notice)
}

/// Every transcript entry rendered for one frame, with blank lines between
/// blocks.
fn transcript_lines(state: &UiState, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut previous: Option<&Entry> = None;
    for entry in &state.entries {
        if let Some(previous) = previous {
            if separates(previous, entry) {
                lines.push(Line::default());
            }
        }
        lines.extend(ui::entry_lines(entry, &state.theme, width, state.tick));
        previous = Some(entry);
    }
    lines
}

fn draw(frame: &mut Frame, state: &mut UiState) {
    let area = frame.area();
    let input_width = (area.width as usize).saturating_sub(2).max(1);
    let input_rows = ui::wrap_text(&state.input, input_width).len() as u16;
    let layout = ui::compute_layout(area, input_rows, state.approval.as_ref());

    if let Some(band) = layout.banner {
        let lines = ui::banner_lines(&state.theme, band.width as usize);
        frame.render_widget(Paragraph::new(lines), band);
    }

    if state.help {
        let lines = ui::help_overlay(
            &state.theme,
            layout.transcript.width as usize,
            layout.transcript.height as usize,
        );
        frame.render_widget(Paragraph::new(lines), layout.transcript);
    } else {
        let lines = transcript_lines(state, layout.transcript.width as usize);
        state.max_scroll = lines
            .len()
            .saturating_sub(layout.transcript.height as usize) as u16;
        let offset = if state.follow {
            state.max_scroll
        } else {
            state.scroll.min(state.max_scroll)
        };
        state.scroll = offset;
        frame.render_widget(Paragraph::new(lines).scroll((offset, 0)), layout.transcript);
    }

    if let (Some(view), Some(band)) = (state.approval.as_ref(), layout.approval) {
        let lines = ui::approval_lines(
            view,
            &state.theme,
            band.width as usize,
            band.height as usize,
        );
        frame.render_widget(Paragraph::new(lines), band);
    }

    let input = ui::input_lines(
        &state.input,
        &state.theme,
        layout.input.width as usize,
        false,
    );
    frame.render_widget(Paragraph::new(input), layout.input);

    let status = ui::status_line(&state.status(), &state.theme, layout.status.width as usize);
    frame.render_widget(Paragraph::new(status), layout.status);

    let hints = ui::hints_line(&state.theme, layout.hints.width as usize);
    frame.render_widget(Paragraph::new(hints), layout.hints);

    // Park the terminal cursor where the next typed character will land.
    let typed = &state.input[..byte_index(state, state.cursor)];
    let wrapped = ui::wrap_text(typed, input_width);
    let row = wrapped.len().saturating_sub(1) as u16;
    let column = ui::text_width(wrapped.last().map(String::as_str).unwrap_or(""));
    let y = layout.input.y + row.min(layout.input.height.saturating_sub(1));
    let x = (layout.input.x + 2 + column as u16)
        .min(layout.input.x + layout.input.width.saturating_sub(1));
    frame.set_cursor_position(Position::new(x, y));
}

/// Render one frame off-screen so the layout can be inspected without a TTY
/// or an API key (`cargo run --bin tui -- --snapshot`, add `--ansi` for color).
fn snapshot(ansi: bool) -> Result<()> {
    let mut state = sample_state();
    let mut terminal = Terminal::new(TestBackend::new(100, 30))?;
    terminal.draw(|frame| draw(frame, &mut state))?;
    let buffer = terminal.backend().buffer();
    for y in 0..buffer.area.height {
        if ansi {
            println!("{}", ansi_line(buffer, y));
        } else {
            println!("{}", render_row(buffer, y, false));
        }
    }
    Ok(())
}

/// A representative scene: a goal, a task, tool calls and a pending gate.
fn sample_state() -> UiState {
    let mut state = UiState::new(
        "claude-sonnet-4-5".to_string(),
        PathBuf::from("D:\\Agent\\Rust-agent"),
        2,
    );
    state.push(Entry::user("/goal the demo loop finishes"));
    state.apply(UiEvent::Notice(
        "[goal] active: the demo loop finishes".into(),
    ));
    state.push(Entry::user("count the rust files"));
    state.apply(UiEvent::AssistantText(
        "Sure - let me look at the workspace first.\n".to_string(),
    ));
    state.apply(UiEvent::ToolCall {
        name: "glob".into(),
        input: "{\"pattern\":\"src/**/*.rs\"}".into(),
    });
    state.apply(UiEvent::ToolResult {
        name: "glob".into(),
        output: "src/agent.rs\nsrc/compact.rs\nsrc/config.rs\n... 34 more".into(),
        is_error: false,
    });
    state.apply(UiEvent::AssistantText(
        "Next, a shell step - watch the permission gate.\n".to_string(),
    ));
    state.apply(UiEvent::ToolCall {
        name: "bash".into(),
        input: "{\"command\":\"echo rm -rf demo\"}".into(),
    });
    state.apply(UiEvent::Approval {
        name: "bash".into(),
        input: "{\"command\":\"echo rm -rf demo\"}".into(),
        reason: "potentially destructive command".into(),
    });
    state.tick = 3;
    state
}

/// One buffer row as text, optionally wrapped in ANSI color escapes.
fn render_row(buffer: &ratatui::buffer::Buffer, y: u16, ansi: bool) -> String {
    let mut out = String::new();
    let mut styled = false;
    for x in 0..buffer.area.width {
        let cell = &buffer[(x, y)];
        if ansi {
            let bold = if cell.modifier.contains(Modifier::BOLD) {
                "1;"
            } else {
                ""
            };
            out.push_str(&format!(
                "\u{1b}[{bold}{};{}m",
                ansi_fg(cell.fg),
                ansi_bg(cell.bg)
            ));
            styled = true;
        }
        out.push_str(cell.symbol());
    }
    if styled {
        out.push_str("\u{1b}[0m");
    }
    out.trim_end().to_string()
}

/// Alias used by the snapshot printer.
fn ansi_line(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    render_row(buffer, y, true)
}

/// A scripted model for `--demo`: text -> glob -> bash (permission gate) -> text.
struct MockClient {
    step: AtomicUsize,
}

impl MockClient {
    fn new() -> Self {
        MockClient {
            step: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl LlmClient for MockClient {
    async fn complete(
        &self,
        request: CompletionRequest<'_>,
    ) -> rust_agent::Result<CompletionResponse> {
        // s17: answer the goal evaluator with a valid verdict, so `/goal` can
        // be demoed (and tested) without an API key.
        if request.system.contains("independent completion evaluator") {
            return Ok(CompletionResponse {
                content: vec![ContentBlock::text(
                    "{\"ok\": true, \"reason\": \"the demo transcript shows the loop finished\", \
                     \"impossible\": false}",
                )],
                stop_reason: Some("end_turn".into()),
                model: Some("demo-model".into()),
                usage: None,
            });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        let (content, stop) = match step {
            0 => (
                vec![
                    ContentBlock::text("Sure - let me look at the workspace first.\n"),
                    ContentBlock::ToolUse {
                        id: "demo-1".into(),
                        name: "glob".into(),
                        input: serde_json::json!({ "pattern": "src/**/*.rs" }),
                    },
                ],
                "tool_use",
            ),
            1 => (
                vec![
                    ContentBlock::text("Next, a shell step - watch the permission gate.\n"),
                    ContentBlock::ToolUse {
                        id: "demo-2".into(),
                        name: "bash".into(),
                        // Flagged as destructive by the policy, but harmless to run.
                        input: serde_json::json!({ "command": "echo rm -rf demo" }),
                    },
                ],
                "tool_use",
            ),
            _ => (
                vec![ContentBlock::text(
                    "Done. That is the whole loop: model -> tool -> model.\n",
                )],
                "end_turn",
            ),
        };
        Ok(CompletionResponse {
            content,
            stop_reason: Some(stop.to_string()),
            model: Some("demo-model".into()),
            usage: None,
        })
    }
}

/// Map a ratatui color to its 8/16-color foreground SGR code.
fn ansi_fg(color: Color) -> u8 {
    match color {
        Color::Black => 30,
        Color::Red => 31,
        Color::Green => 32,
        Color::Yellow => 33,
        Color::Blue => 34,
        Color::Magenta => 35,
        Color::Cyan => 36,
        Color::Gray => 37,
        Color::DarkGray => 90,
        Color::LightRed => 91,
        Color::LightGreen => 92,
        Color::LightYellow => 93,
        Color::LightBlue => 94,
        Color::LightMagenta => 95,
        Color::LightCyan => 96,
        Color::White => 97,
        _ => 39,
    }
}

/// Map a ratatui color to its 8/16-color background SGR code.
fn ansi_bg(color: Color) -> u8 {
    match color {
        Color::Black => 40,
        Color::Red => 41,
        Color::Green => 42,
        Color::Yellow => 43,
        Color::Blue => 44,
        Color::Magenta => 45,
        Color::Cyan => 46,
        Color::Gray => 47,
        Color::DarkGray => 100,
        Color::LightRed => 101,
        Color::LightGreen => 102,
        Color::LightYellow => 103,
        Color::LightBlue => 104,
        Color::LightMagenta => 105,
        Color::LightCyan => 106,
        Color::White => 107,
        _ => 49,
    }
}
