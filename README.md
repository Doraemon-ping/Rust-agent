# rust-agent

A minimal, extensible coding-agent runtime in Rust.

Rust owns the *harness*: the loop, tool dispatch, and the permission gates.
The model owns the *agency*. This crate is modelled on the `agent-loop`
lessons in `learn-claude-code` (s01-s09), re-implemented idiomatically with
`tokio` + `reqwest` + `serde`.

## The loop

Everything reduces to one pattern:

```text
User prompt --UserPromptSubmit--> LLM
                                   |
                    (tool_use?) ---+--- no --> Stop hook? --no--> done
                          |          yes           |
                          |                        +---> LLM (continue)
                          v
                    PreToolUse  (first Some() blocks the call)
                          |
                          v
                        Tool  --> PostToolUse  (observe)
                          |
                          v
                    tool_result  --> LLM
```

```rust
for step in 0..max_steps {
    history = compact(history);            // s08: prepare() before every call
    let response = client.complete(CompletionRequest { messages: history, tools, .. }).await?;
    history.push(Message::assistant(response.content));

    let calls = response.tool_calls();
    if calls.is_empty() {
        return Ok(()); // the model is done
    }

    let results = calls.into_iter().map(|call| run_tool(call)).collect();
    history.push(Message::user_blocks(results));
}
```

That is the whole agent. Everything else is a seam you can replace.

## Layout

| File | Responsibility |
| --- | --- |
| `src/agent.rs` | The loop, the `Agent` runtime, the builder, `AgentEvent` |
| `src/llm/mod.rs` | `LlmClient` trait and the wire types |
| `src/llm/anthropic.rs` | Anthropic Messages API client |
| `src/tools/mod.rs` | `Tool` trait, `ToolRegistry`, workspace-safe `ToolContext` |
| `src/tools/bash.rs` | `bash` tool (platform shell, timeout, output cap) |
| `src/tools/fs.rs` | `read_file`, `write_file`, `edit_file`, `glob` |
| `src/tools/todo.rs` | Stateful `todo_write` task list (s05) |
| `src/tools/skill.rs` | `load_skill` tool: reads a full `SKILL.md` on demand (s07) |
| `src/skills.rs` | `SkillLoader`: scans `skills/*/SKILL.md`, builds the catalog |
| `src/compact.rs` | Four-step context compaction and the `compact` tool (s08) |
| `src/memory.rs` | Durable memory (s09): file-backed store, recall, extract, consolidate |
| `src/permission.rs` | Three-gate pipeline: deny list, rules, approval |
| `src/hooks.rs` | `Hook` trait + `Hooks` registry: `UserPromptSubmit` / `PreToolUse` / `PostToolUse` / `Stop` |
| `src/subagent.rs` | `task` tool: nested agent loop with fresh context (s06) |
| `src/message.rs` | Conversation model (`Message`, `ContentBlock`, `ToolCall`) |
| `src/config.rs` | Environment configuration |
| `src/main.rs` | Interactive REPL |

## Mapping to `learn-claude-code`

| Lesson | Concept | Here |
| --- | --- | --- |
| s01 | The loop + one `bash` tool | `Agent::run_turn` + `tools/bash.rs` |
| s02 | Tool dispatch map | `ToolRegistry` + `Tool` trait |
| s03 | Deny list / rules / approval | `PermissionPolicy` + `Approver` |
| s04 | Hook points around the loop | `Hook` + `Hooks`, `PermissionHook` |
| s05 | Model-maintained task list | `TodoTool` / `TodoList` + a reminder in the loop |
| s06 | Subagent with fresh context | `TaskTool` + `subagent_system_prompt`, `ToolRegistry::base` |
| s07 | Skills: catalog first, content on demand | `SkillLoader` + `load_skill` |
| s08 | Compact: spill, archive, trim, summarise | `Compactor` + `compact` tool |
| s09 | Durable memory: recall, extract, consolidate | `MemoryManager` + `MemoryStore` + `CatalogRecall` |

## Skills

The system prompt carries a *catalog* -- one line per skill -- not the skill
bodies. The model pulls a full `SKILL.md` only when it calls `load_skill`.

```text
  skills/<name>/SKILL.md  -->  SkillLoader  -->  system prompt catalog
                                     ^
  LLM -- load_skill(name) --> full SKILL.md -- tool_result --> LLM
```

Drop a skill at `skills/<name>/SKILL.md`, with optional frontmatter:

```markdown
---
name: code-review
description: Review Rust code. Use when asked to review a change.
---

The full instructions, read only on demand.
```

Then enable it:

```rust
let agent = Agent::builder()
    .client(client)
    .skills(SkillLoader::new("skills"))
    .build();
```

`skills(..)` appends the catalog to the system prompt and registers
`load_skill`. The CLI already scans `<workspace>/skills`.

## Compaction

A long session fills the window with tool results and outputs. Rather than
summarising everything, `Compactor::prepare` runs before every model call and
spends the cheapest loss first:

```text
  1. tool_result_budget   spill results > 30k chars to .task_outputs/tool-results/
  2. snip_compact         > 50 messages: archive the middle to .transcripts/
  3. micro_compact        replace older results with a "saved at" reference
  4. fit_tool_results     preview whatever is still inline
  5. compact_history      summarise the rest (one extra model call)
```

`reactive_compact` is the safety net: when the provider answers
`prompt_too_long`, the oldest messages are summarised and the turn retries once.
The model can also ask for it directly with the `compact` tool, which is
honoured only after the current tool batch has closed.

```rust
let agent = Agent::builder()
    .client(client)
    .compaction(CompactConfig::default())
    .build();
```

`compaction(..)` registers `compact`, runs the pipeline each turn, and emits
`AgentEvent::Compacted { kind, transcript }` so an observer can show what
happened. Every threshold is a field on `CompactConfig`.

## Memory

Compaction forgets; memory remembers. `MemoryManager` keeps durable knowledge
outside the conversation -- as files -- and folds only the relevant records
back into the system prompt for the current turn.

```text
  .memory/MEMORY.md      catalog: one line per record
  .memory/<slug>.md      one record: frontmatter (name/description/type) + body

  recall       every turn    catalog -> model picks indices -> bodies (capped)
  extract      after a turn  dialogue -> candidates -> validate -> write
  consolidate  at >= 10      merge duplicates -> replace the store (with rollback)
```

Recall is a two-stage read: the model picks catalog indices (with a
deterministic lexical fallback), then only those bodies are loaded, capped by
`recall_char_limit`. Extraction and consolidation are separate model calls that
return JSON and are validated before anything is written: a record is stored
only when it is durable, not temporary, and not a duplicate. A failed
consolidation restores the previous store byte for byte.

```rust
let agent = Agent::builder()
    .client(client)
    .memory(MemoryConfig::default())
    .build();
```

`memory(..)` emits `AgentEvent::MemoryRecalled { count }` and
`AgentEvent::MemoryStored { stored, consolidated }`. Every threshold is a field
on `MemoryConfig`; leave it off to disable durable memory entirely. Recall is
itself a `MemoryRecall`, so a vector index can replace `CatalogRecall` without
touching the loop.

## Getting started

```sh
cp .env.example .env      # then fill in ANTHROPIC_API_KEY and MODEL_ID
cargo run
```

Environment variables:

| Variable | Meaning | Default |
| --- | --- | --- |
| `ANTHROPIC_BASE_URL` | Messages API base URL | `https://api.anthropic.com` |
| `ANTHROPIC_API_KEY` | API key (falls back to `ANTHROPIC_AUTH_TOKEN`) | required |
| `MODEL_ID` | Model id (falls back to `ANTHROPIC_MODEL`) | required |
| `MAX_TOKENS` | Response budget per call | `8192` |

In the REPL: type a task, `/reset` to clear history, `q` to quit.

## Embedding

```rust
use rust_agent::{Agent, AnthropicClient, Config};

let client = AnthropicClient::new(&Config::from_env()?);
let agent = Agent::builder()
    .client(client)
    .workspace("/path/to/project")
    .observer(MyObserver)   // render progress
    .approver(MyApprover)   // answer the approval prompt
    .build();

let answer = agent.ask("summarise src/lib.rs").await?;
```

## Extending

- **A new tool**: implement `Tool` and register it with
  `Agent::builder().tools(registry)`. The loop never changes.
- **A new provider**: implement `LlmClient` (one method) and pass it to
  `builder().client(..)`. Local models, OpenAI-compatible gateways, and mocks
  all fit the same seam.
- **Hooks / telemetry**: implement `AgentObserver`; every event has a no-op
  default.
- **Loop callbacks**: implement `Hook` and register it with
  `Agent::builder().hook(..)`. `PreToolUse` returning `Some` blocks a call;
  `Stop` returning `Some` keeps the loop going. The default permission gate is
  itself just a `PreToolUse` hook, so the loop stays free of hard-coded policy.
- **Different policy**: build a `PermissionPolicy` and supply your own
  `Approver`.
- **Planning cadence**: tune or disable the todo reminder with
  `Agent::builder().todo_reminder_after(n)`.
- **Delegation**: wrap a subagent with `TaskTool::new(subagent)` and register
  it on the parent. Give the subagent `ToolRegistry::base()` (no `task` tool)
  and it cannot recurse; only its final text returns.
- **A new skill**: add a `skills/<name>/SKILL.md`; no code change. Use
  `builder().skills(SkillLoader::new(dir))` to point at another directory.
- **Compaction policy**: `builder().compaction(CompactConfig::default())` and
  tune the thresholds, or leave it off entirely. `reactive_compact` reuses the
  same config's `max_reactive_retries`.
- **Memory policy**: `builder().memory(MemoryConfig::default())` and tune the
  thresholds, or leave it off. Swap in a different `MemoryRecall` to change how
  records are selected without touching extraction or consolidation.

## Tests

```sh
cargo test      # loop, hooks, todos, subagents, permissions, escaping, skills, compaction, memory
cargo clippy --all-targets
```
