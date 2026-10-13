//! The `rust-agent` CLI: a small REPL over the agent loop.

use std::io::Write;
use std::sync::Arc;

use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use rust_agent::agent::{AgentEvent, AgentObserver};
use rust_agent::hooks::Hook;
use rust_agent::permission::Approver;
use rust_agent::subagent::{subagent_system_prompt, TaskTool};
use rust_agent::{
    default_system_prompt, Agent, AnthropicClient, BackgroundManager, CompactConfig, Config,
    ContentBlock, CronScheduler, LlmClient, McpRegistry, MemoryConfig, Message, RecoveryConfig,
    SkillLoader, Team, ToolCall, ToolRegistry, BACKGROUND_PROMPT, CRON_PROMPT, MCP_PROMPT,
    TASKS_PROMPT, TEAM_PROMPT,
};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let config = Config::from_env()?;
    let workspace = std::env::current_dir()?;
    let client: Arc<dyn LlmClient> = Arc::new(AnthropicClient::new(&config));

    // The subagent: fresh context, base tools only, so it cannot delegate.
    let subagent = Agent::builder()
        .shared_client(client.clone())
        .tools(ToolRegistry::base())
        .workspace(workspace.clone())
        .max_tokens(config.max_tokens)
        .max_steps(30)
        .system_prompt(subagent_system_prompt())
        .observer(SubagentObserver)
        .approver(InteractiveApprover)
        .build();

    // s10: defaults plus the persistent task graph.
    let mut tools = ToolRegistry::with_tasks();
    tools.register(TaskTool::new(subagent));

    // s07: skills live in `<workspace>/skills/*/SKILL.md`.
    let skills = SkillLoader::new(workspace.join("skills"));
    let skill_count = skills.len();

    // s12: scheduled prompts, restored from `.scheduled_tasks.json`.
    let cron = CronScheduler::new(workspace.clone());
    match cron.load() {
        Ok(0) => {}
        Ok(count) => println!("cron: restored {count} durable job(s)"),
        Err(error) => eprintln!("cron: could not load jobs: {error}"),
    }
    cron.start_ticker();

    // s13: one Lead plus persistent teammates over file mailboxes.
    let team = Team::new(workspace.clone(), client.clone()).with_observer(TeammateObserver);

    // s14: external tools arrive through MCP servers, gated by host policy.
    let mcp = McpRegistry::new();

    // s15: retry transient model failures, fall back, and escalate truncation.
    let mut recovery = RecoveryConfig::default();
    if let Some(model) = &config.fallback_model {
        println!("recovery: fallback model {model}");
        recovery = recovery.with_fallback_model(model.clone());
    }

    let agent = Agent::builder()
        .shared_client(client)
        .tools(tools)
        .workspace(workspace.clone())
        .skills(skills)
        .compaction(CompactConfig::default())
        .memory(MemoryConfig::default())
        .background(BackgroundManager::new())
        .cron(cron.clone())
        .team(team.clone())
        .mcp(mcp.clone())
        .recovery(recovery)
        .max_tokens(config.max_tokens)
        .system_prompt(format!(
            "{} {} {} {} {} {} Use task for focused exploration or a self-contained subtask.",
            default_system_prompt(),
            TASKS_PROMPT,
            BACKGROUND_PROMPT,
            CRON_PROMPT,
            TEAM_PROMPT,
            MCP_PROMPT
        ))
        .observer(PrintingObserver)
        .approver(InteractiveApprover)
        .hook(LargeOutputWarning)
        .hook(SessionSummary)
        .build();

    println!(
        "rust-agent -- model: {} | workspace: {}",
        config.model,
        workspace.display()
    );
    println!("skills: {skill_count} loaded from skills/");
    println!("memory: .memory/ (recall + extract + consolidate)");
    println!("tasks: .tasks/ (create, update, claim, complete)");
    println!("background: bash run_in_background -> <task_notification> (s11)");
    println!("cron: .scheduled_tasks.json (schedule_cron, list_crons, cancel_cron) (s12)");
    println!("team: .mailboxes/ (spawn_teammate, send_message, review_plan) (s13)");
    println!("mcp: connect_mcp -> mcp__<server>__<tool> (docs, deploy) (s14)");
    println!("recovery: 429/529 backoff, fallback model, max_tokens escalation (s15)");
    println!("Type a task, or /reset to clear history, or q to quit.\n");

    let mut editor = DefaultEditor::new()?;
    let mut history: Vec<Message> = Vec::new();

    loop {
        // A job that came due while we were idle starts its own turn before
        // the next prompt: the loop polls the scheduler as it begins.
        if cron.has_due() {
            println!("\x1b[35m[cron] a scheduled job is due\x1b[0m");
            if let Err(error) = agent.run_turn(&mut history).await {
                eprintln!("\x1b[31merror: {error}\x1b[0m");
            }
            println!();
        }

        // A teammate event wakes the Lead on its own, exactly like a due job.
        if team.bus().peek(rust_agent::team::LEAD).unwrap_or(false) {
            println!("\x1b[35m[team] teammate events arrived\x1b[0m");
            if let Err(error) = agent.run_turn(&mut history).await {
                eprintln!("\x1b[31merror: {error}\x1b[0m");
            }
            println!();
        }

        let line = match editor.readline("rust-agent >> ") {
            Ok(line) => line,
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            Err(error) => return Err(error.into()),
        };

        let query = line.trim();
        match query {
            "" => continue,
            "q" | "quit" | "exit" => break,
            "/reset" => {
                history.clear();
                println!("history cleared\n");
                continue;
            }
            _ => {}
        }

        let _ = editor.add_history_entry(query);
        history.push(Message::user_text(agent.prepare_prompt(query)));

        if let Err(error) = agent.run_turn(&mut history).await {
            eprintln!("\x1b[31merror: {error}\x1b[0m");
        }
        println!();
    }

    Ok(())
}

/// Renders agent events to the terminal.
struct PrintingObserver;

impl AgentObserver for PrintingObserver {
    fn on_event(&self, event: AgentEvent<'_>) {
        match event {
            AgentEvent::AssistantText(text) => print!("{text}"),
            AgentEvent::ToolCall { name, input } => {
                println!("\n\x1b[36m> {name}\x1b[0m {}", compact(input));
            }
            AgentEvent::ToolResult {
                name,
                output,
                is_error,
            } => {
                if name == "todo_write" && !is_error {
                    println!("\x1b[33m## Current Tasks\x1b[0m\n{}", preview(output));
                } else if name == "list_tasks" && !is_error {
                    println!("\x1b[33m## Task Graph\x1b[0m\n{}", preview(output));
                } else {
                    let color = if is_error { "\x1b[31m" } else { "\x1b[90m" };
                    println!("{color}{}\x1b[0m", preview(output));
                }
            }
            AgentEvent::Blocked { name, reason } => {
                println!("\x1b[31m[blocked] {name}: {reason}\x1b[0m");
            }
            AgentEvent::Compacted { kind, transcript } => match transcript {
                Some(path) => println!(
                    "\x1b[33m[compact:{kind}] transcript: {}\x1b[0m",
                    path.display()
                ),
                None => println!("\x1b[33m[compact:{kind}]\x1b[0m"),
            },
            AgentEvent::MemoryRecalled { count } => {
                println!("\x1b[33m[memory: recalled {count}]\x1b[0m");
            }
            AgentEvent::MemoryStored {
                stored,
                consolidated,
            } => {
                if consolidated > 0 {
                    println!(
                        "\x1b[33m[memory: stored {stored}, consolidated to {consolidated}]\x1b[0m"
                    );
                } else {
                    println!("\x1b[33m[memory: stored {stored}]\x1b[0m");
                }
            }
            AgentEvent::BackgroundStarted { task_id } => {
                println!("\x1b[35m[background] {task_id} running\x1b[0m");
            }
            AgentEvent::BackgroundCollected { count } => {
                println!("\x1b[35m[background] collected {count} result(s)\x1b[0m");
            }
            AgentEvent::CronDelivered { count } => {
                println!("\x1b[35m[cron] delivered {count} job(s)\x1b[0m");
            }
            AgentEvent::TeamEventDelivered { count } => {
                println!("\x1b[35m[team] {count} event(s) from teammates\x1b[0m");
            }
            AgentEvent::McpConnected { server, tools } => {
                println!("\x1b[35m[mcp] connected {server}: {tools} tool(s)\x1b[0m");
            }
            AgentEvent::ModelRecovered {
                kind,
                attempt,
                delay_ms,
            } => {
                println!("\x1b[33m[{kind}] retry {attempt} after {delay_ms}ms\x1b[0m");
            }
            AgentEvent::ModelFellBack { model } => {
                println!("\x1b[31m[529] switching to {model}\x1b[0m");
            }
            AgentEvent::MaxTokensEscalated { max_tokens } => {
                println!("\x1b[33m[max_tokens] retry with {max_tokens}\x1b[0m");
            }
            AgentEvent::ContinuationRequested { count } => {
                println!("\x1b[33m[continuation {count}] asking the model to continue\x1b[0m");
            }
            AgentEvent::Step(_) => {}
        }
    }
}

/// Asks the user before running a flagged tool.
struct InteractiveApprover;

/// Renders a running teammate's own tool traffic, dimmed and prefixed.
struct TeammateObserver;

impl AgentObserver for TeammateObserver {
    fn on_event(&self, event: AgentEvent<'_>) {
        match event {
            AgentEvent::ToolCall { name, input } => {
                println!("\x1b[35m  [team] > {name}\x1b[0m {}", compact(input));
            }
            AgentEvent::ToolResult {
                name,
                output,
                is_error,
            } => {
                let color = if is_error { "\x1b[31m" } else { "\x1b[35m" };
                let body = preview(output).replace('\n', "\n  ");
                println!("{color}  [team] {name}: {body}\x1b[0m");
            }
            AgentEvent::Blocked { name, reason } => {
                println!("\x1b[31m  [team] blocked {name}: {reason}\x1b[0m");
            }
            _ => {}
        }
    }
}

/// Renders a subagent's own events, prefixed and dimmed.
struct SubagentObserver;

impl AgentObserver for SubagentObserver {
    fn on_event(&self, event: AgentEvent<'_>) {
        match event {
            AgentEvent::ToolCall { name, .. } => println!("\x1b[35m  [sub] > {name}\x1b[0m"),
            AgentEvent::ToolResult {
                name,
                output,
                is_error,
            } => {
                let color = if is_error { "\x1b[31m" } else { "\x1b[35m" };
                let head: String = output
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(100)
                    .collect();
                println!("{color}  [sub] {name}: {head}\x1b[0m");
            }
            AgentEvent::Blocked { name, reason } => {
                println!("\x1b[31m  [sub] blocked {name}: {reason}\x1b[0m");
            }
            AgentEvent::AssistantText(_)
            | AgentEvent::Step(_)
            | AgentEvent::Compacted { .. }
            | AgentEvent::MemoryRecalled { .. }
            | AgentEvent::BackgroundStarted { .. }
            | AgentEvent::BackgroundCollected { .. }
            | AgentEvent::CronDelivered { .. }
            | AgentEvent::TeamEventDelivered { .. }
            | AgentEvent::McpConnected { .. }
            | AgentEvent::ModelRecovered { .. }
            | AgentEvent::ModelFellBack { .. }
            | AgentEvent::MaxTokensEscalated { .. }
            | AgentEvent::ContinuationRequested { .. }
            | AgentEvent::MemoryStored { .. } => {}
        }
    }
}

impl Approver for InteractiveApprover {
    fn approve(&self, call: &ToolCall, reason: &str) -> bool {
        println!("\n\x1b[33m[permission] {reason}\x1b[0m");
        println!("  tool: {} {}", call.name, compact(&call.input));
        print!("  allow? [y/N] ");
        let _ = std::io::stdout().flush();

        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

/// One-line, length-capped preview of tool input.
fn compact(value: &serde_json::Value) -> String {
    let text = value.to_string();
    if text.chars().count() <= 200 {
        text
    } else {
        text.chars().take(200).collect::<String>() + "..."
    }
}

/// First few lines of a tool result.
fn preview(text: &str) -> String {
    let total = text.lines().count();
    let mut out = text.lines().take(20).collect::<Vec<_>>().join("\n");
    if total > 20 {
        out.push_str("\n...");
    }
    out
}

/// `PostToolUse` hook: warn when a tool produces a very large result.
struct LargeOutputWarning;

impl Hook for LargeOutputWarning {
    fn post_tool_use(&self, call: &ToolCall, output: &str) {
        if output.len() > 100_000 {
            println!(
                "\x1b[33m[hook] large output from {}: {} chars\x1b[0m",
                call.name,
                output.len()
            );
        }
    }
}

/// `Stop` hook: print a one-line session summary when the model stops.
struct SessionSummary;

impl Hook for SessionSummary {
    fn stop(&self, history: &[Message]) -> Option<String> {
        let tool_results = history
            .iter()
            .flat_map(|message| message.content.iter())
            .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
            .count();
        println!("\x1b[90m[hook] stop: {tool_results} tool results this session\x1b[0m");
        None
    }
}
