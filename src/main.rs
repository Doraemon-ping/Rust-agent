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
    default_system_prompt, Agent, AnthropicClient, CompactConfig, Config, ContentBlock, LlmClient,
    MemoryConfig, Message, SkillLoader, ToolCall, ToolRegistry,
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

    let mut tools = ToolRegistry::with_defaults();
    tools.register(TaskTool::new(subagent));

    // s07: skills live in `<workspace>/skills/*/SKILL.md`.
    let skills = SkillLoader::new(workspace.join("skills"));
    let skill_count = skills.len();

    let agent = Agent::builder()
        .shared_client(client)
        .tools(tools)
        .workspace(workspace.clone())
        .skills(skills)
        .compaction(CompactConfig::default())
        .memory(MemoryConfig::default())
        .max_tokens(config.max_tokens)
        .system_prompt(format!(
            "{} Use task for focused exploration or a self-contained subtask.",
            default_system_prompt()
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
    println!("Type a task, or /reset to clear history, or q to quit.\n");

    let mut editor = DefaultEditor::new()?;
    let mut history: Vec<Message> = Vec::new();

    loop {
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
            AgentEvent::Step(_) => {}
        }
    }
}

/// Asks the user before running a flagged tool.
struct InteractiveApprover;

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
