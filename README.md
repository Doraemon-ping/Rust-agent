# rust-agent

A minimal, extensible coding-agent runtime in Rust.

Rust owns the *harness*: the loop, tool dispatch, and the permission gates.
The model owns the *agency*. This crate is modelled on the `agent-loop`
lessons in `learn-claude-code` (s01-s17), re-implemented idiomatically with
`tokio` + `reqwest` + `serde`.

## The loop

Everything reduces to one pattern:

```text
User prompt --UserPromptSubmit--> LLM
                                   |
                    (tool_use?) ---+--- no --> Stop hooks, then the goal evaluator?
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
    let mut calls = Vec::new();
    // Text reaches the observer the moment it arrives, and a tool call is
    // collected as soon as its block closes -- the response assembles under it.
    let response = client
        .stream(CompletionRequest { messages: history, tools, .. }, &mut |event| match event {
            StreamEvent::Text(text) => observer.on_event(AgentEvent::AssistantText(&text)),
            StreamEvent::ToolCall(call) => calls.push(call),
        })
        .await?;
    history.push(Message::assistant(response.content));

    if calls.is_empty() {
        // s17: /goal re-enters the loop until its condition is met
        if let GoalDecision::Block { reason } = goal.evaluate_after_turn(&history, running).await {
            history.push(Message::user_text(reason));
            continue;
        }
        return Ok(()); // the model is done
    }

    let results = calls.into_iter().map(|call| run_tool(call)).collect();
    history.push(Message::user_blocks(results));
}
```

That is the whole agent. Everything else is a seam you can replace.

### Model calls stream

The loop never calls `complete` directly. It calls `LlmClient::stream`, which
reports text and finished tool calls as they arrive and returns the assembled
response; the trait's default implementation calls `complete` and replays the
finished blocks, so a provider (or mock) that cannot stream still works
unchanged. The Anthropic client implements it for real: `stream: true`, the SSE
body parsed line by line, each `text_delta` forwarded the moment it lands -- so
the first token is visible before the response ends -- and each `tool_use`
block collected as soon as its block closes. A stream failure that lands after
part of the response was already shown is reported instead of retried:
replaying it would duplicate text the user has already seen. (A response cut
off by `max_tokens` is still retried with a bigger budget, exactly as before,
so its partial text may briefly show twice before the fuller answer replaces
it.)

## Layout

| File | Responsibility |
| --- | --- |
| `src/agent.rs` | The loop, the `Agent` runtime, the builder, `AgentEvent` |
| `src/llm/mod.rs` | `LlmClient` trait (`complete` / streaming `stream`) and the wire types |
| `src/llm/anthropic.rs` | Anthropic Messages API client, streaming over SSE |
| `src/tools/mod.rs` | `Tool` trait, `ToolRegistry`, workspace-safe `ToolContext` |
| `src/tools/bash.rs` | `bash` tool (platform shell, timeout, output cap) |
| `src/tools/fs.rs` | `read_file`, `write_file`, `edit_file`, `glob` |
| `src/tools/todo.rs` | Stateful `todo_write` task list (s05) |
| `src/tools/task.rs` | Persistent task graph: `create_task` / `update_task` / `claim_task` / `complete_task` (s10) |
| `src/tools/skill.rs` | `load_skill` tool: reads a full `SKILL.md` on demand (s07) |
| `src/skills.rs` | `SkillLoader`: scans `skills/*/SKILL.md`, builds the catalog |
| `src/compact.rs` | Four-step context compaction and the `compact` tool (s08) |
| `src/memory.rs` | Durable memory (s09): file-backed store, recall, extract, consolidate |
| `src/background.rs` | Background Bash tasks (s11): worker threads + `<task_notification>` injection |
| `src/cron.rs` | Cron scheduler (s12): 5-field expressions, durable jobs, `[Scheduled]` delivery |
| `src/team.rs` | Agent teams (s13): mailboxes, teammate runtimes, plan gates, worktrees |
| `src/mcp.rs` | MCP tools (s14): connect servers, namespace discovered tools, host policy |
| `src/recovery.rs` | Model-call recovery (s15): classify 429/529, backoff, fallback model, escalation |
| `src/workflow.rs` | Workflow runtime (s16): saved scripts, `agent`/`parallel`/`pipeline`, journal, resume |
| `src/goal.rs` | Goal loop (s17): a session condition plus an independent, tool-free evaluator at the Stop boundary |
| `src/session.rs` | Durable conversations: one JSON file per session in a store directory (`<workspace>/.sessions` by default, `--sessions-dir` to put it elsewhere), with the writer's own view kept verbatim |
| `src/permission.rs` | Three-gate pipeline: deny list, rules, approval |
| `src/hooks.rs` | `Hook` trait + `Hooks` registry: `UserPromptSubmit` / `PreToolUse` / `PostToolUse` / `Stop` |
| `src/subagent.rs` | `task` tool: nested agent loop with fresh context (s06) |
| `src/message.rs` | Conversation model (`Message`, `ContentBlock`, `ToolCall`) |
| `src/config.rs` | Environment configuration |
| `src/main.rs` | Interactive REPL |
| `src/bin/tui.rs` | Experimental `ratatui` TUI front-end over the same agent |
| `src/bin/desktop_bridge.rs` | JSON Lines bridge the Tauri desktop shell drives (`desktop/README.md`) |
| `src/ui.rs` | Pure render helpers for the TUI (`Entry`, `Theme`, layout, wrapping) |

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
| s10 | Persistent task graph with `blockedBy` + `owner` | `TaskStore` + the six task tools |
| s11 | Slow Bash commands leave the loop | `BackgroundManager` + `run_in_background` + `<task_notification>` |
| s12 | A prompt starts at a scheduled local time | `CronScheduler` + `schedule_cron` / `list_crons` / `cancel_cron` |
| s13 | Lead delegates to persistent teammates | `Team` + `MessageBus` + `spawn_teammate` and the protocol tools |
| s14 | Connect a server, discover and call its tools | `McpRegistry` + `McpServer` + `connect_mcp` + `mcp__{server}__{tool}` |
| s15 | Many mechanisms, one loop; recover from provider failures | the whole `Agent` + `RecoveryConfig` + `Agent::builder().interactive(..)` |
| s16 | A saved script orchestrates the subagents | `WorkflowRegistry` + `WorkflowTool` + `ExecutionContext` + `WorkflowJournal` |
| s17 | Stay in the loop until an independent evaluator agrees | `GoalController` + `PromptGoalEvaluator` + `/goal` |

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

## Tasks

`todo_write` is a checklist for the current session. The task system (s10) is
the durable version: each task is a JSON file under `<workspace>/.tasks/`, the
`blockedBy` edges form a dependency graph, and `owner` records who claimed it.
Because it lives on disk, progress is recoverable across sessions.

```text
create_task ─► pending ─claim─► in_progress ─complete─► completed
                                                            │
                        unblocks downstream tasks ◄─────────┘
```

The model builds the graph in two phases: create every node, then wire the
edges with the ids `create_task` returned (a tool result cannot feed its own
arguments, so ids only exist after the create call returns).

| Tool | Effect |
| --- | --- |
| `create_task` | New `pending` task; returns its `task_xxxxxxxx` id |
| `update_task` | Add `blockedBy` edges (pending + unowned only; self/cycle rejected) |
| `list_tasks` | One line per task: status marker, owner, dependencies |
| `get_task` | Full JSON, for recovering a description across sessions |
| `claim_task` | `pending` → `in_progress`, once every dependency is complete |
| `complete_task` | `in_progress` → `completed`, reporting what it unblocked |

```rust
let agent = Agent::builder()
    .client(client)
    .tools(ToolRegistry::with_tasks())   // defaults + the six task tools
    .system_prompt(format!("{} {}", default_system_prompt(), TASKS_PROMPT))
    .build();
```

## Background tasks

A slow command need not block the loop. When the model calls `bash` with
`run_in_background: true` (s11), the loop hands the command to a worker thread
and answers the call at once with a placeholder result:

```text
Turn 1: bash "npm install" (run_in_background=true)
        -> tool_result "[Background task bg_0001 started]"
Turn 2: read_file "package.json"      (runs while the install churns)
Turn 3: -> <task_notification> for bg_0001 is injected with the result
```

The finished result arrives on a later turn as a `<task_notification>` block.
It deliberately does *not* reuse the original `tool_use_id`: that call was
already answered by the placeholder, and one `tool_use` maps to exactly one
`tool_result`.

```rust
let agent = Agent::builder()
    .client(client)
    .background(BackgroundManager::new())   // advertises run_in_background
    .system_prompt(format!("{} {}", default_system_prompt(), BACKGROUND_PROMPT))
    .build();
```

The parameter is explicit: the harness never guesses from keywords such as
`install` or `build`. `background(..)` emits `AgentEvent::BackgroundStarted`
and `AgentEvent::BackgroundCollected`; tune the per-command limits with
`BackgroundManager::with_timeout(..)` / `with_max_output(..)`.

## Scheduled prompts

The scheduler (s12) answers "run the tests every weekday at 09:00" without the
user having to ask again. A job pairs a 5-field cron expression with a prompt;
when the expression matches the local clock the prompt is queued, and the loop
delivers it as a `[Scheduled] <prompt>` user message on its next turn.

```text
  .scheduled_tasks.json        poll(now)          take_due()
  cron:   0 9 * * 1-5  ---->   due?    ---->  [Scheduled] run tests
  prompt: run tests                                  |
                                          acknowledge / restore
```

Expressions are the usual five fields -- `minute hour day month weekday` --
with `*`, `*/N`, `N`, `N-M`, and comma-separated lists; `validate_cron`
rejects a bad expression before anything is stored, and day-of-month and
day-of-week combine with cron's OR rule.

| Tool | Effect |
| --- | --- |
| `schedule_cron` | Register a job; returns its `cron_xxxxxxxx` id |
| `list_crons` | One line per job: expression, frequency, storage |
| `cancel_cron` | Remove a job by id |

```rust
let agent = Agent::builder()
    .client(client)
    .cron(CronScheduler::new(workspace))   // registers the three tools
    .system_prompt(format!("{} {}", default_system_prompt(), CRON_PROMPT))
    .build();
```

Delivery is at-least-once. A job is acknowledged only after the model has
accepted the prompt, so a failed call removes the `[Scheduled]` message again
and puts the job back in the queue. `durable` jobs are written to
`.scheduled_tasks.json` (temp file + rename) and restored by
`CronScheduler::load`; `recurring=false` removes a job once delivered.
`start_ticker()` polls once a second on a background thread so a job fires
while the agent sits idle -- delivery still happens at the next turn
boundary. `cron(..)` emits `AgentEvent::CronDelivered`.

## Agent teams

A subagent (s06) is one call. A *teammate* (s13) is a long-lived runtime: its
own conversation, its own working directory, and a `WORK -> IDLE -> WORK` loop
over the shared task board. The two agents never share a `messages` array, so
they coordinate through file mailboxes instead.

```text
  Lead ──spawn_teammate──> TeammateRuntime (thread)
    ^                          │  work / idle scan
    │  .mailboxes/lead.jsonl    │  claims ready tasks, completes them
    └── report / idle / plan / shutdown ──┘
```

| Tool | Effect |
| --- | --- |
| `spawn_teammate` | Claim an initial task, then start one persistent teammate |
| `list_teammates` | One line per teammate: `name: status` |
| `send_message` | A plain message to `lead` or an active teammate |
| `request_shutdown` / `review_plan` | Answer the two typed protocols |
| `request_plan` | Gate a teammate behind a plan review |
| `create_worktree` | `git worktree add .worktrees/<name>` and bind it to a task |

Teammate-side tools are `send_message` and `submit_plan`, plus the base file and
shell tools and `list_tasks` / `claim_task` / `complete_task` -- the last two
claim *for that teammate*, so ownership stays honest on the shared board.

The plan gate rides on the existing hook pipeline: while a teammate's gate is
`required`, `pending`, or `rejected`, a `PreToolUse` hook blocks `bash`,
`write_file`, and `edit_file`, and the model is told why. Approval is a typed
request: `plan_approval_request` carries a `req_NNNNNN` id, `review_plan`
answers it, and the teammate accepts a response only if its id, sender, target,
status, and work version still match. Claiming, releasing, or completing a task
bumps the work version, so a stale approval can never unlock a new assignment.

```rust
let team = Team::new(workspace, client);
let agent = Agent::builder()
    .client(client)
    .team(team.clone())                    // registers the Lead's tools
    .system_prompt(format!("{} {}", default_system_prompt(), TEAM_PROMPT))
    .build();
```

`Agent::run_turn` drains the Lead's inbox before each model call and folds the
events in as `[Team events] ...`, emitting `AgentEvent::TeamEventDelivered`.
The CLI does the same while the REPL is idle, so a teammate's result wakes the
Lead on its own. Teammates are threads with their own tokio runtime; a
teammate's working directory is fixed when it is spawned (its task's worktree,
or the repository), so idle auto-claiming only takes tasks that resolve to that
same directory. Removing a worktree stays with the host, as in the reference.

A teammate keeps the Lead's context small, and the Lead's wakeups rare:

- **One report per turn.** The `complete_task` tool records what it closed
  instead of messaging the Lead itself; at the end of the turn the runtime
  sends a single structured `task_completed` message per turn -- the task ids
  plus the model's closing prose -- so a finished task never also arrives as a
  separate `result` line. A turn that closes two tasks still sends one report.
- **No idle spin.** A teammate spawned without a task parks on its mailbox
  instead of spending a model turn announcing that it is ready; it calls the
  model only once it has a claimed task or a message. Give a teammate its
  instruction as a task (or a message) rather than in the spawn prompt alone.
- **Quiet while it works.** The idle announcement is only sent when the
  teammate is actually going to wait. If a successor task is already ready it
  claims it and keeps going, so a chain does not wake the Lead between steps.
- **No todo reminder.** Teammates run with `todo_reminder_after(0)`: their plan
  is the shared task board, and the reminder would only add unused context.

## MCP tools

The base tools are compiled in; external services are not. An MCP *server*
advertises a `tools/list` catalogue and a `tools/call` entry point, and the
harness decides what to do with it (s14). The model starts with the built-in
tools plus `connect_mcp`; connecting a server adds its tools to the pool for
the *next* model call.

```text
  connect_mcp(name="docs")
        |  tools/list
        v
  mcp__docs__search, mcp__docs__get_version   <- namespaced, in the pool
        |  tools/call
        v
  handler -> tool_result -> LLM
```

Tools are namespaced `mcp__{server}__{tool}`, with both halves normalized to
the model's tool-name alphabet, so `docs.one/get.version` and
`docs_one/get_version` cannot silently map to the same name. The host policy
table decides authorization -- a `readOnlyHint` in a server's description is a
hint, never a permission:

| Server / tool | Policy |
| --- | --- |
| `docs/search`, `docs/get_version`, `deploy/status` | `allow` |
| `deploy/trigger` | `confirm` |
| anything else | `confirm` |

```rust
let agent = Agent::builder()
    .client(client)
    .mcp(McpRegistry::new())               // registers connect_mcp
    .system_prompt(format!("{} {}", default_system_prompt(), MCP_PROMPT))
    .build();
```

`mcp(..)` registers `connect_mcp`, gates discovered tools behind
`McpPermissionHook` (a `confirm` tool pauses at the same `Approver` as a
destructive command), lists connected servers in the system prompt, and emits
`AgentEvent::McpConnected { server, tools }`. A malformed catalogue or a name
collision is rejected at connect time, so the per-turn pool never half-fails.
The shipped `docs` and `deploy` servers are in-process mocks; a real transport
is just an `McpServer` implementation handed to `McpRegistry::register_server`.

## Recovery and asynchronous turns

Everything above hangs off one loop, which is the s15 point: the mechanisms are
seams, not separate programs. Two seams exist purely to keep that loop alive.

`recovery(..)` wraps the model call. Transient failures are retried before they
reach the caller:

```text
  429 rate limited   -> exponential backoff + jitter, retry (up to max_retries)
  529 overloaded     -> exponential backoff, and after N in a row switch model
  max_tokens         -> escalate the budget once, then ask the model to continue
  prompt too long    -> reactive compact, then retry (s08)
  anything else      -> surfaced to the caller untouched
```

Backoff doubles from `base_delay_ms`, caps at `max_delay_ms`, and adds up to 25%
jitter so clients do not retry in lockstep. A truncated response is retried once
with `escalated_max_tokens`; if it is still truncated and has nothing to run, the
partial answer is kept and `CONTINUATION_PROMPT` asks for the rest (bounded by
`max_continuation_retries`). The fallback model needs a per-request override, so
`CompletionRequest::model` carries it.

The second seam is approval. A turn that runs on its own thread -- a teammate --
has no terminal to prompt on, so it is built with `interactive(false)`. Anything
the policy would have *asked* about is denied instead, with a reason the model
can read, rather than blocking on input nobody can answer:

```rust
let agent = Agent::builder()
    .client(client)
    .recovery(RecoveryConfig::default().with_fallback_model("claude-haiku-4-5"))
    .interactive(false)          // deny instead of asking
    .build();
```

`recovery(..)` emits `AgentEvent::ModelRecovered`, `ModelFellBack`,
`MaxTokensEscalated`, and `ContinuationRequested`; `Config::fallback_model`
comes from `FALLBACK_MODEL_ID`.

## Workflow runtime

Every round so far is model-driven: the model reads the tool results and picks
the next step. That is right when the next step depends on what the last one
found. Some tasks instead repeat a fixed procedure -- review four dimensions,
verify each finding, keep the ones that survive, sort by severity -- and the
host already knows the steps and their order. `builder().workflow(registry)`
exposes those saved procedures as one `Workflow` tool.

The model supplies a **name** and args, never code. Behind a single `tool_use`
a host script coordinates the subagent calls with four primitives:

| Primitive | Meaning |
| --- | --- |
| `agent(prompt, schema?, label?, phase?)` | run one subagent, optionally constrained to a JSON schema |
| `parallel(thunks)` | **barrier**: run all of them, return results in input order |
| `pipeline(items, stages)` | per-item staged flow, **no barrier** between stages |
| `phase(title)` / `log(message)` | progress markers, a phase is announced once |

Each `agent()` call is journalled under a *semantic* key -- a hash of
`kind|label|prompt|schema`, never a call counter -- so concurrent completion
order cannot shift the keys. `Workflow(name, resume_from_run_id: ..)` replays
the journalled results and only re-runs the steps that changed. With a
`schema`, an answer that fails validation is re-asked once before it becomes an
error. A `budget` arg caps the run's tokens; `AGENT_CAP` caps `agent()` calls.

Intermediate values live in the script's variables; only the final result and a
task summary go back to the conversation:

```json
{ "launched": { "taskId": "local_workflow_wf_review-changes_1a2b3c4d5e6f7a8b", "runId": "wf_review-changes_1a2b3c4d5e6f7a8b", "workflowName": "review-changes" },
  "result":   { "confirmed": [ { "dimension": "security", "title": "...", "severity": "high" } ] },
  "task":     { "status": "completed", "usage": { "agents": 9, "tokens": 1204 }, "progress": [ ".." ] } }
```

A run keeps its data in `<workspace>/.workflow/`: a snapshot (`<runId>.json`), an
output file (`<runId>.output.json`), the journal (`<runId>.journal.jsonl`), and
`last_run.txt` naming the most recent run. Lifecycle and progress lines
(`async_launched`, `task_started`, `workflow_phase`, `workflow_agent`,
`task_notification`) reach the observer as `AgentEvent::WorkflowReported`.

The built-in `review-changes` workflow runs a `pipeline` over four review
dimensions: four auditors, then each finding verified adversarially by its own
subagent, then the confirmed findings sorted by severity. Swap in a mock runner
and the whole plan is deterministic and offline:

```rust
let registry = rust_agent::default_registry()?
    .with_runner(Arc::new(AnthropicAgentRunner::new(client.clone())));
let agent = Agent::builder().client(client).workflow(registry).build();
```

## Goal loop

One exit condition has stood since s01: the model stops calling tools, so the
turn returns. That is right for ordinary conversation, but wrong for "keep
fixing until every test passes" -- no `tool_use` proves only that *this* turn
ended, not that the whole task is done. `builder().goal(controller)` adds one
independent check at that boundary.

```text
/goal pytest tests/auth exits 0 and lint reports no errors
```

The condition is stored for the session and handed to the model as the current
task. When the model stops asking for tools, the goal Stop hook runs inside the
same loop:

```rust
let decision = goal.evaluate_after_turn(history, background_running).await;
if let GoalDecision::Block { reason } = decision {
    history.push(Message::user_text(goal_continuation(condition, reason)));
    continue;                     // the same loop, no second entry point
}
return Ok(());
```

| Decision | Meaning |
| --- | --- |
| `Allow` | no active goal: stop exactly as in s01 |
| `Defer` | a background command is still running; skip the evaluator, keep the goal |
| `Block` | the condition is not met: append the reason and keep looping |
| `Achieved` / `Failed` | the condition was met, or judged impossible; the goal is cleared |
| `Limit` | the Stop hook blocked `DEFAULT_STOP_HOOK_BLOCK_CAP` turns in a row |
| `Error` | the evaluator call failed; the goal stays active and the error surfaces |

The evaluator is a **separate model call with no tools**. It sees only the
condition and the conversation -- a transcript of recent messages, with tool
calls and results rendered inline, and only an oversized newest message trimmed
-- so it judges the evidence the worker actually surfaced. It cannot read a
file or rerun a test. `PromptGoalEvaluator` asks for exactly one JSON object:

```json
{ "ok": false, "reason": "The conversation does not contain pytest's exit code yet.", "impossible": false }
```

`ok` must be a bool, `reason` a non-empty string, `impossible` a bool, and never
both `ok` and `impossible`. A fenced code block is tolerated; anything else is
an evaluator error, which keeps the goal active rather than claiming success.

In the REPL:

| Command | Effect |
| --- | --- |
| `/goal` | show the condition, elapsed time, evaluations, tokens, and last reason |
| `/goal <condition>` | set or replace the goal and start working immediately |
| `/goal clear` | clear it (`stop`, `off`, `reset`, `none`, `cancel` also work) |

Two exits live *outside* the goal: the loop's global `max_steps`, and the
consecutive-block cap. Either one returns control to the user with the goal
still active -- it is never silently marked complete or cleared. A defer is
resumed when a finished background command wakes the session, and its result
enters the conversation like any other message before the evaluator judges it.

```rust
let goal = GoalController::with_defaults(Arc::new(
    PromptGoalEvaluator::from_env(client.clone()),   // GOAL_EVALUATOR_MODEL_ID
));
let agent = Agent::builder().client(client).goal(goal.clone()).build();
```

`GoalController::restore(evaluator, events, block_cap)` revives a still-active
goal from the `goal_status` records a host persisted: the condition carries
over, while count, elapsed time, and the token baseline start fresh. Transitions
reach the observer as `AgentEvent::GoalReported`.

## Getting started

```sh
cp .env.example .env      # then fill in ANTHROPIC_API_KEY and MODEL_ID
cargo run                 # the TUI (the default binary)
cargo run --bin rust-agent   # the line-oriented REPL
```

Environment variables:

| Variable | Meaning | Default |
| --- | --- | --- |
| `ANTHROPIC_BASE_URL` | Messages API base URL | `https://api.anthropic.com` |
| `ANTHROPIC_API_KEY` | API key (falls back to `ANTHROPIC_AUTH_TOKEN`) | required |
| `MODEL_ID` | Model id (falls back to `ANTHROPIC_MODEL`) | required |
| `FALLBACK_MODEL_ID` | Model used after repeated `529`s (s15) | none |
| `GOAL_EVALUATOR_MODEL_ID` | Cheaper model for the goal evaluator (s17) | the worker's model |
| `MAX_TOKENS` | Response budget per call | `8192` |

In the REPL: type a task, `/goal <condition>` to keep working until it is met,
`/reset` to clear history, `q` to quit.

## TUI (experimental)

```sh
cargo run                 # the TUI (the default binary)
# no key handy? run the scripted demo model:
cargo run -- --demo
# or render one frame to text (add --ansi for color):
cargo run -- --snapshot
```

A `ratatui` front-end over the *same* agent the CLI builds: skills, compaction,
memory, subagent, task graph, background commands, cron, teams, MCP, recovery,
workflows, and the `/goal` loop. The agent runs on its own thread with a
current-thread Tokio runtime, so the blocking approval prompt never freezes the
screen; every `AgentEvent` streams into a scrolling transcript, and because the
loop forwards text a delta at a time, prose appears as the model writes it. The
visuals live in `src/ui.rs` as pure functions over plain data, so `src/bin/tui.rs`
is just an event/input shell and the rendering is unit-tested without a terminal.

Type a task and press Enter, use `/goal <condition>` (or `/goal` for status and
`/goal clear`) to drive the goal loop, and answer permission prompts with `y`
(once), `a` (always, for the session) or `n`. `Shift+Enter` inserts a newline,
`?` or `/help` shows the key map, and `Esc` quits. While the UI is idle the
agent thread keeps polling, so a due cron job, an incoming teammate message, or
a finished background command starts its own turn -- which is how a deferred
goal is judged as soon as its evidence arrives. `--demo` swaps in a scripted mock
model (it answers the goal evaluator too), so the UI, the permission gate, and
the goal loop can all be exercised without an API key.

## Desktop shell

```sh
cd desktop
pnpm install
pnpm app:dev        # a Tauri 2 + Vue 3 window over this same runtime
```

`desktop/` hosts this crate as a child process over a JSON Lines bridge
(`src/bin/desktop_bridge.rs`) rather than reimplementing the agent: it decides
nothing the runtime already knows. The Settings page writes the endpoint, the
API key, and the model into a host-owned `provider.json`, and the shell hands
them to the runtime as environment variables when it spawns it. The Chat page
drives the same agent loop: it sends a prompt as `chat.send` and renders the
turn the runtime streams back as `chat.*` events, re-reading them with
`chat.history` when the window is reloaded, ending a turn early with
`chat.cancel`, and moving between the conversations the runtime keeps on disk
with `sessions.list`, `sessions.new`, `sessions.open`, `sessions.rename`, and
`sessions.delete`. The shell keeps those conversations in its own storage under
the per-user app data directory, one store per workspace, so a project folder is
never also a folder the runtime writes into. Protocol revision, layout, and
platform status live in `desktop/README.md`.

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
- **A new provider**: implement `LlmClient` and pass it to `builder().client(..)`.
  `complete` is the only required method; implement `stream` as well to send
  text deltas and finished tool calls as they arrive. Local models,
  OpenAI-compatible gateways, and mocks that do neither still fit the seam.
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
- **Completion criteria**: set a `/goal` condition, or install a controller
  with `Agent::builder().goal(..)`. Implement `GoalEvaluator` to judge the
  transcript with your own rule -- a test report parser, a checklist, another
  model. The loop is untouched either way.
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
- **Teams**: `builder().team(Team::new(workspace, client))` registers the Lead
  tools and folds teammate events into the turn. `MessageBus` and the gate
  protocols are separate seams, so a different transport or approval scheme
  does not touch the loop.
- **External tools**: `builder().mcp(McpRegistry::new())` registers
  `connect_mcp` and the `mcp__*` host policy. Implement `McpServer` over any
  transport and hand it to `McpRegistry::register_server`; the pool, the
  permission hook, and the loop are unchanged.
- **Provider resilience**: `builder().recovery(RecoveryConfig::default())`
  retries `429`/`529`, switches models, and escalates truncation. Every knob is
  a field on `RecoveryConfig`; leave it off and the loop calls the provider
  exactly once per round.
- **Asynchronous approval**: `builder().interactive(false)` turns "ask the user"
  into "deny, with a reason" -- for turns with no terminal to prompt on.
- **Saved workflows**: `builder().workflow(registry)` registers the `Workflow`
  tool. Register trusted scripts with `registry.register(meta, script)`; the
  model only ever passes a name and args. `WorkflowRegistry::set_runner` swaps
  the step runner (mock, another model, a fixed fixture) without touching a
  script.

## Tests

```sh
cargo test      # loop, hooks, todos, subagents, permissions, escaping, skills, compaction, memory, tasks, background, cron, teams, mcp, recovery, workflows, goals
cargo clippy --all-targets
```
