# rust-agent desktop workbench

A Tauri 2 + Vue 3 shell that hosts the `rust-agent` runtime as a child process
and talks to it over JSON Lines on stdin/stdout.

**Phase 1 scope.** This is infrastructure, not product. It proves the shell can
start the real runtime, correlate requests, stream events, show honest status,
and shut down without leaving orphans. It also ships the shell's chrome: an icon
rail of destinations, a project column backed by a real folder picker, a chat
page that drives the runtime's own loop, a 定时任务 page over the runtime's own
schedule, 技能 / 插件 pages over the runtime's loader and MCP registry, and a
任务 page over the same `.tasks` graph the model's task tools write. Departments,
their agents, and a group chat that can start a department shift all live on the
部门 page: the rail lists what exists.

## Prerequisites

- Node.js 20+ and pnpm
- A Rust toolchain (the shell and the runtime share one)
- Platform webview deps: WebView2 (Windows), WKWebView (macOS, bundled), or
  `webkit2gtk-4.1` / `libayatana-appindicator3-dev` / `librsvg2-dev` (Linux)

## Commands

Run everything from `desktop/`.

```bash
pnpm install            # frontend deps
pnpm bridge:build       # build the runtime bridge and stage it as a sidecar
pnpm app:dev            # dev shell: Vite on :1420 + hot reload, host starts the runtime
pnpm app:build          # packaged app (builds the frontend and a release sidecar)
pnpm typecheck          # vue-tsc
pnpm build              # frontend bundle only
pnpm icons              # regenerate platform icons from app-icon.png
```

`pnpm app:dev` and `pnpm app:build` differ only in profile. `app:build` runs
`pnpm build && pnpm bridge:build --release` first, so the packaged app carries a
release sidecar.

The bridge itself lives in the parent crate as `src/bin/desktop_bridge.rs`, so
the runtime keeps one Cargo manifest and one build. `scripts/build-bridge.mjs`
builds it for the **host triple only** and copies it to
`src-tauri/binaries/rust-agent-bridge-<triple>[.exe]`, which is the name Tauri's
bundler expects for an `externalBin`.

## App icon

`app-icon.png` is the artwork the app ships with: a 1024x1024 PNG whose flat
white backdrop was knocked out to transparency, leaving only the rounded
sticker art. `pnpm icons` runs the Tauri icon generator over it and rewrites
every size under `src-tauri/icons/` -- `icon.ico`, `icon.icns`, the PNG ladder,
and the Windows Store logos.

The source keeps its original ~6% margin, which is roughly what macOS and
Windows expect, so the generator runs with defaults and no extra padding is
applied. Replace `app-icon.png` and re-run `pnpm icons` to change the icon, then
rebuild: `src-tauri/build.rs` watches the generated icon files, because the
Windows icon is embedded by the build script and cargo would otherwise keep a
stale resource. Do not hand-edit the files under `src-tauri/icons/`.

## Layout

Three columns, left to right:

| column | contents |
| --- | --- |
| rail (`src/components/AppRail.vue`) | 部门 / 任务 / 定时任务 / 插件 / 技能, the app mark, the runtime's health dot, and 设置 |
| projects (`src/components/ProjectPanel.vue`) | 添加项目 (the system folder picker), the stored projects, which one is active, and a runtime state badge |
| work (`src/views/`) | the routed page: 对话, 部门, 任务, 定时任务, 插件, 技能, or 设置 |

The rail lists only destinations that exist, and every one of them is a page
with the runtime's own facts on it.

## Projects

A project is a folder the user picked, and it means more than a bookmark: it is
the workspace the runtime is started in, so the runtime keeps its memories,
tasks, transcripts, and logs inside it. With no project selected, the runtime
uses the per-user app data workspace as before.

- The list lives in `projects.json` under the app data directory
  (`src-tauri/src/projects.rs`). The page reads it from the host, never from
  browser storage: there is no localStorage copy that could drift from what the
  runtime will actually use.
- The page cannot name a folder. `projects_pick` opens the system picker from
  Rust (`tauri-plugin-dialog`) and stores what the user chose; the page can only
  select or remove ids the host already holds.
- Choosing a project while the runtime is live restarts it so it moves
  workspace. Choosing one while the runtime is stopped does not start it:
  picking a project is not a request to run anything.
- A project whose folder was deleted is flagged (`目录缺失`) and kept -- the
  host does not delete what the user added. Starting the runtime in it fails
  with `workspace_missing` rather than quietly running somewhere else.
- A `projects.json` this build cannot read is reported and never rewritten:
  mutations refuse until the file is dealt with, so a parse bug cannot eat the
  list.

## Model settings

The Settings page is where the endpoint, the API key, and the model are entered
(`src/components/ProviderSettingsCard.vue`, host side
`src-tauri/src/provider.rs`). The runtime reads all of it from its process
environment, so the shell owns the environment of the child it starts:

- A save writes `provider.json` under the app data directory, and the next
  spawn sets `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`, `MODEL_ID`,
  `FALLBACK_MODEL_ID`, and `MAX_TOKENS` from it. A field left empty is not set
  at all, so the runtime's own default applies instead of a frozen copy of it.
- The key is write-only. The page can store one, clear one, and ask whether one
  is stored; `provider_get` answers with `hasKey` and the last four characters,
  never the value. On Unix the file is written `0600`.
- Saving while the runtime is live restarts it, because the environment only
  exists per process. Saving while it is stopped starts nothing: a settings
  change, like choosing a project, is not a request to run anything.
- A `provider.json` this build cannot read is reported and never rewritten,
  exactly like the project list.
- The runtime also reads a `.env` in its workspace, the way its other binaries
  read one from their working directory. `dotenvy` never overwrites a variable
  that is already set, so what Settings saved wins and a project's `.env` fills
  in only what is still missing: a project cannot silently redirect a runtime
  the UI believes it configured.

## Chat

The Chat page drives the same `Agent` loop the CLI does, through the bridge
rather than around it (`ChatView.vue` -> `stores/chat.ts` -> `lib/chat`). One
prompt is one turn: the store pushes a pending turn, the bridge accepts it and
answers with the turn id, and the turn's `chat.*` events are folded back onto
that turn as they arrive. The turn id can arrive after the first delta, so a
turn the store cannot match by id falls back to the one prompt that is pending.

The runtime is what holds the conversation, so the page only ever renders it. A
reloaded window is a new page but not a new conversation: the page reads the
turns back with `chat.history` and replays each through the folding the live
events go through, so a reload in the middle of an answer picks it up where it
was. An event that arrives before the turn it names is held rather than dropped,
for the same reason.

The conversation is also written down as it goes, so it outlives the process
that held it. Where it is written is the host's decision rather than the
project's: the host names a store under the per-user app data directory, one
directory per workspace, so a folder the runtime works in is never also a folder
the runtime writes into. Restarting the runtime -- or choosing another project,
which restarts it -- does not end it: the runtime continues the conversation it
was last holding, and a page that reloads reads it back the way it reads back a
reload. The conversation picker on the chat page lists what this workspace
holds, marks the one being continued, starts a fresh one, picks up an older one,
and renames or deletes any of them.

- The agent is built on the first prompt, not at startup, so a runtime with no
  model configured still starts and still answers for itself. A prompt sent
  before one is configured is refused with `chat_not_configured`, before it
  takes the one-turn slot, so a refusal never blocks a later prompt.
- The header says how full the window is. The runtime compacts a long
  conversation on its own before every model call, and nothing about that is
  visible unless it is asked for: the page reads `chat.context` after each turn
  for the size the compactor measures, the limit it measures against, and how
  often it has already run, and draws a compaction notice inline where the
  runtime reports one -- so a history that suddenly shrank has a reason on the
  page rather than only in the log.
- A turn that needs a decision waits for the window. There is no terminal
  behind the window, so the third permission gate asks the page instead: a
  call the policy flags is reported as `chat.permission` with the id it
  carries, the turn parks inside that call, and `chat.approve` (`{id,
  allow}`) decides it -- allow runs the call, refuse denies it with the
  reason. The runtime reports what it decided as `chat.permission_result`, so
  what the page shows is what happened rather than what was pressed.
- A turn nobody is watching does not park. A scheduled turn says so before it
  starts, so a flagged call there is still denied with a reason and shown as
  `chat.blocked`, exactly as before. The same is true of a `task` call's own
  loop: a subagent's turn is not the user's, so it cannot stop and wait.
- The model can put a question to the user. `ask_user` carries a question and
  at most a few suggested answers; it is reported as `chat.question`, answered
  with `chat.answer` (`{id, answer}`) in the user's own words or with one of
  the suggestions, and the answer comes back as `chat.question_result`. A
  blank answer is a real answer -- nobody replied -- and the model is told
  that rather than handed an empty string. Only the conversation gets the
  tool: the subagent above does not, so a nested loop cannot ask the user.
- An interaction never outlives its turn. Stopping a turn lets go of whatever
  it was parked on, and a turn that ends closes any request still open, so the
  page is never left waiting on a person nobody is asking any more.
- A failed turn is rolled back: the half exchange is dropped rather than left
  for the next prompt to trip over. The failure carries a code the page branches
  on and the provider's own words.
- A running turn can be stopped. 发送 becomes 停止 while a turn is in flight; a
  stop ends the turn where it stands, so whatever streamed stays on the page,
  `chat.cancelled` names the turn that stopped, and the half exchange is dropped
  rather than fed back to the model. The stop is answered only once the turn is
  really over, so the prompt sent straight after pressing it is accepted instead
  of being refused as a turn still running.
- Switching conversations is refused while a turn is running, and the page says
  so rather than dropping the turn: the one-turn slot is the runtime's, not the
  page's, and a conversation cannot be swapped out from under it.
- The loop keeps its history for the life of the runtime process, and a `task`
  call's subagent reports its tool calls but not its prose -- that prose is the
  `task` tool's result to the model, not an answer to the user.
- Work the runtime does after the turn's answer (`memory.after_turn` and the
  like) is not streamed: the turn is over by the time it runs. It still lands in
  the workspace's own stores.

## How the pieces fit

```
Vue view  ->  Pinia store  ->  lib/runtime + lib/projects + lib/provider + lib/chat
          ->  Tauri command (commands.rs)
          ->  RuntimeManager (runtime.rs)  ->  child process stdin
child stdout  ->  reader task  ->  status + events  ->  emit  ->  store  ->  view
```

**Frontend services** (`src/lib/runtime/index.ts`, `src/lib/projects/index.ts`,
`src/lib/provider/index.ts`, `src/lib/chat/index.ts`, `src/lib/cron/index.ts`,
`src/lib/tasks/index.ts`, `src/lib/departments/index.ts`, `src/lib/skills/index.ts`,
`src/lib/plugins/index.ts`)
are the only modules that import `@tauri-apps/api`, and `src/lib/shell.ts` holds
the one guard they share. The runtime seam exposes `getRuntimeStatus`,
`startRuntime`, `stopRuntime`, `restartRuntime`, `pingRuntime`,
`getRuntimeLogs`, `getHostInfo`, `subscribeRuntimeEvents`, and
`unsubscribeRuntimeEvents`. The projects seam exposes `listProjects`,
`pickProject`, `selectProject`, and `removeProject`. The provider seam exposes
`getProviderSettings` and `saveProviderSettings`. The cron seam exposes
`fetchCronJobs`, `scheduleCronJob`, and `cancelCronJob`; the tasks seam exposes
`fetchTasks`, `createTask`, `claimTask`, `releaseTask`, and `completeTask`; the
departments seam exposes `fetchDepartments`, `createDepartment`,
`removeDepartment`, `addAgent`, `removeAgent`, `runDepartment`, and
`stopDepartment`; the
skills seam exposes `fetchSkills`, `readSkill`, and `importSkills`; the plugins
seam exposes `fetchPlugins` and `connectPlugin`. The chat seam exposes `sendChatPrompt`,
`cancelChatTurn`, `fetchChatHistory`, `fetchChatContext`, `fetchChatSessions`,
`newChatSession`, `openChatSession`, `renameChatSession`, `deleteChatSession`,
and `subscribeChatEvents`; a prompt is the one thing the page chooses there, and
the answer arrives on the events rather than the command.

**Host commands** (`src-tauri/src/commands.rs`) are thin wrappers:
`runtime_status`, `runtime_start`, `runtime_stop`, `runtime_restart`,
`runtime_ping`, `runtime_logs`, `runtime_host_info`, `projects_list`,
`projects_pick`, `projects_select`, `projects_remove`, `provider_get`,
`provider_save`, `chat_send`, `chat_cancel`, `chat_history`, `chat_context`,
`chat_sessions`, `chat_new_session`, `chat_open_session`, `chat_rename_session`,
`chat_delete_session`, `cron_list`, `cron_schedule`, `cron_cancel`,
`tasks_list`, `tasks_create`, `tasks_claim`, `tasks_release`, `tasks_complete`,
`departments_list`, `departments_create`, `departments_remove`, `agents_add`,
`agents_remove`, `departments_run`, `departments_stop`,
`skills_list`, `skills_read`, `skills_import`, `plugins_list`, and
`plugins_connect`. None of them takes a path, a binary, or a command line
from the caller, and the commands that name something
the runtime holds -- a project, a conversation -- take an id the runtime itself
handed out. `provider_save` is the one command that takes a secret in -- and its
answer still does not contain one. `chat_send` takes the prompt text and nothing
else; the workspace, the model, and the credentials are all decided in the host.
`chat_open_session` takes a session id, and only one this workspace holds: the
runtime checks its shape before it touches the disk, and answers
`session_unknown` for an id that is well formed but not there.
`chat_rename_session` adds the name to give it, and `chat_delete_session` takes
nothing but the id.

**Host events** are `runtime://status`, `runtime://message`, `runtime://log`,
and `runtime://chat`. Status is pushed on every state change; the page never
polls and never infers. A turn streams on `runtime://chat` alone, kept apart
from the status ring so a long answer cannot flush a state change out of it.

**Where the binary comes from** (`src-tauri/src/paths.rs`) is decided in exactly
one place, in this order: `RUST_AGENT_BRIDGE` (debug builds only), the bundled
sidecar next to the executable, then the sibling cargo target directory so
`tauri dev` works without packaging. The frontend cannot name an executable or
pass it a command line. There is no shell capability in
`capabilities/default.json` beyond `core:event:default`; the folder picker is
opened by the host from Rust, so the page is granted no dialog permission
either. It can ask for a picker, not for a directory.

**Where conversations live** is decided there too. `sessions_dir` puts them under
the per-user app data directory, at `<app data>/sessions/<key>`, where `<key>` is
the workspace's folder name plus a hash of its whole path -- readable at a
glance, and still one store per workspace when two projects share a folder name.
The runtime is told with `--sessions-dir`, so it never writes conversations into
the project it is working in, and switching projects switches conversation sets
with it. Conversations written by an older build, which kept them in
`<workspace>/.sessions`, are not moved for the user: copy the `.json` files into
the store directory to keep them.

## Tasks

The 任务 page is a window onto the runtime's own `TaskStore` -- the same
`<workspace>/.tasks` directory the model's `create_task` / `claim_task` /
`complete_task` tools write -- through `TasksView.vue` -> `stores/tasks.ts` ->
`lib/tasks`. It lists every node with its owner, its prerequisites, and whether
those prerequisites leave it ready, and it can add a node, put it behind
prerequisites the page read from the board, and claim, release, or complete what
the page itself owns.

- The id is always the runtime's. The page names a subject, a description, and
  ids it already read; it never invents an id or a path.
- The page claims as `user`, which no model turn uses (`agent` or a teammate's
  name). Only a `user` task offers 完成 / 放回, so a task a turn is working on is
  shown as that owner's and left alone.
- `ready` is derived rather than stored: pending, unowned, and every
  prerequisite complete. Claiming a blocked node is `task_blocked`, with the
  open ids in the message, and claiming a node in any other state is
  `task_not_ready`.
- `tasks.create` checks each prerequisite before it writes anything, so a typo
  is `task_unknown` (or `task_invalid` for a malformed id) rather than an
  orphan node nobody asked for.
- Completion goes through the store's own path, so the answer names the nodes
  that just became ready -- the same sentence the model sees from
  `complete_task`.
- Nothing is polled: the page asks when it is entered, when the runtime comes
  up, and after a change it made. No task action takes the one-turn slot, so
  the board can be read or moved while the model is answering.

## Departments

The 部门 page is a window onto `<workspace>/.departments` through
`DepartmentsView.vue` -> `stores/departments.ts` -> `lib/departments`. A
department is a durable group with a brief, shared rules, a parent id (so
sub-departments form a tree), a roster, and a group chat. Tasks created by a
member are stamped with the department id and the creating agent id.

- `departments.create` takes a name, description, rules, and an optional
  `parent_id`; the id is generated by the runtime. Removing a parent removes the
  sub-departments below it, but leaves task files alone.
- `agents.add` / `agents.remove` add or remove one member and keep names unique
  within the department. The runtime refuses both while that department's shift
  is running, because the roster file is also where the shift records reports.
- `departments.run` appends the user's instruction to the group chat and starts
  one shift. Shifts are sequential across the workspace: every member works from
  the same `.tasks` directory, so two shifts at once would make the board
  disagree with itself between a claim and its completion. `departments.stop`
  asks the current shift to stop.
- A shift gives each member, in roster order, a tool pool scoped to its
  department and owner id: `create_task` stamps the department and creator,
  `list_tasks` / `get_task` / `update_task` see only that department, and
  `claim_task` / `complete_task` act as that agent. The member is told to claim
  one ready task or create one, do the work, verify it, complete it, and leave
  specific follow-ups if the work is larger than one shift.
- The group chat, each member's last status, created/completed counts, current
  agent, and shift progress are read back from the same files. The page refreshes
  on `department.*` events and keeps a slow poll as a fallback while a shift is
  running.

## Cron

The 定时任务 page is a window onto the runtime's own `CronScheduler` -- the same
job table the model writes to with `schedule_cron` -- through
`CronView.vue` -> `stores/cron.ts` -> `lib/cron`. It lists every job with its
expression, its prompt, whether it repeats and whether it is durable, when it
next runs, and whether it is already waiting to be delivered; it adds jobs and
removes them by id. Nothing is kept on the page's side: a refresh asks the
runtime what it holds, and the count in `runtime.status` is the count in the same
table, because it is the same table.

- A job is created with an expression the runtime validates (`validate_cron`),
  so one it cannot parse is refused with the runtime's own words
  (`cron_invalid`) rather than stored and silently never run.
- The page never names a job id of its own. `cron.cancel` takes one the runtime
  handed out, checked against the `cron_<8 hex>` shape before it is looked up,
  and an id this workspace does not hold is `cron_unknown`.
- 下一次运行 comes from `next_match` in the runtime, computed against the local
  clock, so what the page counts down to is when the scheduler will actually
  fire -- a leap-day expression included.
- A job that is already due shows 待投递 rather than a countdown that has run
  out: it is waiting for the one turn slot, and the chat page is where the turn
  it becomes is visible.
- The page asks the runtime again when a next run it was showing has passed,
  once per deadline, rather than polling on a timer.

The desktop app has no idle loop of its own -- the CLI's REPL and the TUI both
wake the agent when work arrives on its own -- so the bridge runs the clock: a
ticker polls the local clock every second and, when a job is due and no turn is
running, starts a turn of its own. That turn carries no prompt from the host; the
loop injects the `[Scheduled] <prompt>` message and acknowledges the job exactly
as it does in the CLI, so there is one delivery path and at-least-once delivery
is preserved. The turn holds the one-turn slot, can be stopped with 停止, and
streams to the chat page like any other turn. A due job with no model configured
is reported once on stderr and left in the queue: there is nothing to hand it to.

## Skills

The 技能 page is a window onto the runtime's own `SkillLoader` -- the same
catalogue the system prompt carries -- through `SkillsView.vue` ->
`stores/skills.ts` -> `lib/skills`. It lists every `skills/<name>/SKILL.md` the
loader found, with the description the model sees and the manifest it came from,
and opens one manifest in full.

- Nothing is kept on the page's side: `skills.list` re-scans the directory, so a
  skill added or deleted on disk shows up on the next refresh.
- `skills.read` takes a name and hands back the whole manifest -- the same text
  `load_skill` gives the model. The page names a skill, never a path, and a name
  the loader does not hold is `skill_unknown`.
- `skills.import` is the way in: it takes `{"path"}` and copies the skill folder
  there -- or every folder under it that holds a `SKILL.md` -- into
  `<workspace>/skills/`, then reads the directory back so the new ones are
  listed at once. It never overwrites: a name already in the workspace is
  `skill_exists`, a folder with no manifest anywhere is `skill_invalid`, and a
  path that is not a folder is `skill_source`. The 技能 page opens a folder picker
  and calls it, so a skill from anywhere on disk gets in without a hand copy.
- There is no enable/disable: a skill is there because a file is there, so the
  only thing that turns one off is deleting or moving its directory.

## Plugins

A plugin is an MCP server, and the 插件 page is a window onto the runtime's own
`McpRegistry` -- the same registry the agent's tool pool reads -- through
`PluginsView.vue` -> `stores/plugins.ts` -> `lib/plugins`. It lists every server
this build ships and every one the workspace declares, whether each is connected,
and, for the connected ones, the tools discovered from it with the host policy
for each.

- A catalogue only exists once a server is connected: `tools/list` is something
  the runtime has to ask for, so an unconnected plugin shows no tools and says
  why rather than guessing at what it would offer.
- The page connects a server by name (`plugins.connect`); a name this build does
  not ship is `plugin_unknown` before anything is built. The model can connect
  the same servers with `connect_mcp`, and both paths share one registry, so a
  plugin the page installs is in the pool the next model call sees.
- The policy on each tool (`allow` / `confirm`) is the host's decision, from
  `host_policy`, not the server's own annotation. A `confirm` tool in the
  conversation goes through the same gate as any other call: it is reported as
  `chat.permission` and runs only if `chat.approve` allows it. In a turn with
  nobody to ask -- a `task` call's own loop, or a scheduled turn -- it is
  refused with a reason instead of being left waiting.
- There is no disconnect: the runtime's registry only grows within a process, so
  a connected plugin stays connected until the runtime restarts. The page says
  so instead of offering a button that would not work.
- A server this build does not ship comes from the workspace: a `plugins.json` at
  the workspace root names the stdin/stdout command to run (see the 插件 page's
  扩展插件 section), and the runtime reads it at startup, spawns the child, runs
  its `initialize` handshake, and reads its `tools/list`. Those entries carry
  `source: "workspace"` and the `command` that will run, and their tools take the
  `policy` the file names instead of the host default. The file is read once, so
  adding a server means restarting the runtime.

## Protocol (v1)

One JSON object per line, UTF-8, `\n`-framed. **stdout carries protocol only**;
every diagnostic goes to stderr as `{"ts","level","target","message"}`. A line
over the frame limit is dropped and the stream resynchronises at the next
newline. The limit is per direction, and the two are not the same number: a
request over 1 MiB is dropped by the runtime, and a reply over 8 MiB by the
host. The reply limit is the larger one because `chat.history` is the one answer
that grows with the conversation -- it carries the runtime's whole 4 MiB of
turns (see `chat.history` below) plus the envelope around them -- and a reply
the host drops is a conversation the page cannot read back at all.

Request (host to runtime):

```json
{"protocol_version":1,"type":"request","request_id":"host-3","method":"runtime.ping","params":{}}
```

Response, correlated by `request_id`:

```json
{"protocol_version":1,"type":"response","request_id":"host-3","ok":true,"result":{"pong":true,"uptime_ms":41}}
```

Event (asynchronous; never matched against a pending request):

```json
{"protocol_version":1,"type":"event","event":"runtime.ready","ts":"...","data":{...}}
```

Unattributable failure, when there is no request to blame:

```json
{"protocol_version":1,"type":"error","ts":"...","error":{"code":"invalid_json","message":"..."}}
```

Methods: `runtime.status` (a full snapshot), `runtime.ping`, `runtime.shutdown`,
`chat.send`, `chat.cancel`, `chat.history`, `chat.context`, `chat.answer`,
`chat.approve`, `sessions.list`,
`sessions.new`, `sessions.open`, `sessions.rename`, `sessions.delete`,
`cron.list`, `cron.schedule`, `cron.cancel`, `tasks.list`, `tasks.create`,
`tasks.claim`, `tasks.release`, `tasks.complete`, `departments.list`,
`departments.create`, `departments.remove`, `agents.add`, `agents.remove`,
`departments.run`, `departments.stop`, `skills.list`, `skills.read`,
`skills.import`, `plugins.list`, and `plugins.connect`. Only `chat.send`,
`chat.answer`, `chat.approve`,
`sessions.open`, `sessions.rename`, `sessions.delete`, `cron.schedule`,
`cron.cancel`, `tasks.create`, `tasks.claim`, `tasks.release`, `tasks.complete`,
`departments.create`, `departments.remove`, `agents.add`, `agents.remove`,
`departments.run`, `departments.stop`, `skills.read`, `skills.import`, and
`plugins.connect` take
parameters -- the others refuse any key at all, so a host
that thinks it is talking to a newer bridge finds out at once. `chat.send`'s
request carries the prompt and nothing else:

```json
{"protocol_version":1,"type":"request","request_id":"host-9","method":"chat.send","params":{"prompt":"summarise the workspace"}}
```

It answers as soon as the turn is accepted, naming the turn every event of that
prompt will carry:

```json
{"protocol_version":1,"type":"response","request_id":"host-9","ok":true,"result":{"accepted":true,"turn":"turn-1"}}
```

The turn itself arrives as events, each tagged with its `turn` and its `source`
(`main` for the conversation, `subagent` for a `task` call's own loop):
`chat.delta` (the answer, as it is produced), `chat.tool_call`,
`chat.tool_result`, `chat.permission` and `chat.question` (the turn has
stopped inside a call that needs the page -- see below), `chat.blocked` (a
permission-sensitive call refused because there was nobody to ask),
`chat.notice` (progress that is not an
answer -- compaction, memory, recovery, cron), and finally exactly one of
`chat.done` (with the whole answer), `chat.failed` (with an `error`), or
`chat.cancelled` (the turn was stopped).

`chat.history` takes no parameters and answers with the turns reported so far,
oldest first. Each one carries the prompt as it was typed, its status (`running`,
`done`, `failed` or `cancelled`), the code and the provider's words when it
failed, and the very events it arrived as -- so a page can render a conversation
it did not watch. The log is bounded: oldest turns go first once it outgrows 50
turns or 4 MiB, and a turn that is still running is never dropped. The answer is
one line, so that budget and the host's reply limit are chosen together: a
conversation the runtime keeps has to be one the host will take.

`chat.context` takes no parameters and answers with the size of the conversation
in hand as the compactor measures it -- `chars` (`Compactor::estimate_chars`),
`char_limit` (`context_char_limit`, or `null` before a model is configured),
`messages`, and `compacted`, how many times the loop has shrunk the history this
run. It is a fact about the conversation, not the model, so it answers whether or
not the runtime can reach a provider; the Chat page reads it after each turn to
show how full the window is and when the runtime compacted it on its own.

`chat.cancel` takes no parameters and answers with whether a turn was stopped. A
stop that finds nothing running answers `{"cancelled":false}` on the spot; a stop
that reaches a turn is answered by that turn, once it has stopped and the slot it
held is free. That is what makes the two usable in sequence: the prompt the page
sends on the back of the answer is taken, rather than refused as a turn still
running.

`chat.answer` takes `{"id", "answer"}`: the id of the question the turn is
parked on, and the words to hand the model. A blank `answer` is accepted and
means nobody replied. `chat.approve` takes `{"id", "allow"}`: the id of the
call the turn is parked on, and whether it may run. Both answer
`{"accepted": bool}` -- whether the reply reached a call that was still
waiting, which is the honest answer for the ones that arrive after it is over
-- and both refuse a missing or mistyped parameter with `invalid_params`
before anything is delivered. Neither is ever answered twice, and neither
writes anything into the transcript: the request and its outcome arrive as the
four events above, so a page cannot show an answer the model never got.

Conversations are written to `<id>.json` in a directory the host names with
`--sessions-dir`; without it they go to `<workspace>/.sessions`, where the
CLI-driven runtime has always kept them. The desktop host passes a directory
under the per-user app data directory, one per workspace, so a project stays the
project's. The runtime continues the newest conversation it finds when it starts.
`sessions.list` takes no parameters and answers with what that store holds,
newest first, plus the id being continued:

```json
{"protocol_version":1,"type":"response","request_id":"host-11","ok":true,"result":{"current":"s1750000000000-4821-0","sessions":[{"id":"s1750000000000-4821-0","title":"say hello","created_at":"2026-10-14T05:12:00.123456700+00:00","updated_at":"2026-10-14T05:14:31.765432100+00:00","messages":6}]}}
```

The conversation in hand is listed even when it has no file yet -- a 新会话 that
has not run a turn -- so the page can always see which conversation it is in,
and the id in `current` is always one of the entries. A conversation is only
written once it has a turn worth keeping: an empty one is not a file, and a
window that never sent a prompt leaves nothing behind. It does, therefore, not
survive a restart until its first turn has finished -- though a name given to one
is kept, and goes out with that first turn.

`sessions.new` takes no parameters and starts a fresh conversation, leaving the
old one on disk; `sessions.open` takes the id of one this workspace holds and
continues it, numbering its turns after the ones it already has. `sessions.rename`
takes `{"id","title"}` and gives a conversation the name the user typed -- trimmed,
at most 120 characters, and empty is allowed because it means "unnamed" -- so a
conversation that has a file is renamed in that file, and one that has none is
named in memory. `sessions.delete` takes `{"id"}` and answers
`{"deleted","current"}`. Deleting the conversation in hand is allowed: the runtime
then continues the newest conversation that is left, or starts an empty one when
nothing is left, because the next prompt still has to have somewhere to go.
Deleting a conversation that was never written and is not the one in hand is
`session_unknown`. All four refuse while a turn is running. A turn the file caught
mid-flight -- the process was killed, not stopped -- reads back as `failed` with
`chat_interrupted` rather than left spinning, because whatever was going to finish
it is gone.

`cron.list` takes no parameters and answers with every job the runtime holds,
each with the minute it next runs in:

```json
{"protocol_version":1,"type":"response","request_id":"host-13","ok":true,"result":{"pending_delivery":0,"jobs":[{"id":"cron_1a2b3c4d","cron":"0 9 * * 1-5","prompt":"weekday standup","recurring":true,"durable":true,"pending_delivery":false,"last_fired":"2026-10-14 09:00","next_run":"2026-10-15T09:00:00","next_run_ms":64800000}]}}
```

`next_run` is local wall-clock without a zone, because the runtime computed it
from this machine's clock; `next_run_ms` is the distance to it, for a countdown.
Both are `null` when the expression has no next match inside the runtime's
four-year window, and for a one-shot that has already fired and is waiting to be
delivered -- it is on its way to the model, not scheduled for later.

`cron.schedule` takes `{"cron","prompt","recurring","durable"}` and answers
`{"job": {...}}` in the same shape `cron.list` uses; `recurring` and `durable` are
optional booleans defaulting to `true`, and a flag that is present but is not a
boolean is `invalid_params` rather than quietly taken for the default. `cron.cancel`
takes `{"id"}` and answers `{"cancelled": <id>}`. Neither takes the one-turn slot: a
job can be added or removed while a turn is running, because neither goes near the
conversation.

`tasks.list` takes no parameters and answers with the `.tasks` directory and
every node on the board. Each node keeps the field names the store uses on disk,
plus the state its edges imply (`blocked` and `ready`):

```json
{"protocol_version":1,"type":"response","request_id":"host-17","ok":true,"result":{"dir":"D:\\work\\.tasks","count":2,"tasks":[{"id":"task_1a2b3c4d","subject":"write the plan","description":"","status":"completed","owner":"user","blockedBy":[],"worktree":null,"blocked":[],"ready":false},{"id":"task_5e6f7a8b","subject":"ship it","description":"","status":"pending","owner":null,"blockedBy":["task_1a2b3c4d"],"worktree":null,"blocked":[],"ready":true}]}}
```

`tasks.create` takes `{"subject","description"}` and optional
`{"blocked_by": [<task ids>]}`, and answers `{"task": {...}}` in the same shape
`tasks.list` uses. The id is generated by the runtime. Every prerequisite is
checked before the node is written: a malformed id is `task_invalid`, and an id
the board does not hold is `task_unknown`.

`tasks.claim`, `tasks.release`, and `tasks.complete` take `{"id"}` and answer
`{"task": {...}}`, with a `message` for the two actions that report one. Claiming
records `user` as the owner and requires a ready node (`task_blocked` when a
prerequisite is open, `task_not_ready` in any other state); releasing and
completing require the task to be in progress and owned by `user`
(`task_not_owned` otherwise). A claim that loses a race with another writer is
`task_conflict` rather than a success the board does not show. None of the three
takes the one-turn slot.

`departments.list` takes no parameters and answers with the `.departments`
directory and every department, including its roster, group chat, task counts,
and the shift currently driving it:

```json
{"protocol_version":1,"type":"response","request_id":"host-18","ok":true,"result":{"dir":"D:\\work\\.departments","count":1,"departments":[{"id":"dept_1a2b3c4d","name":"研发部","description":"build the thing","instructions":"small changes","parentId":null,"createdAt":"...","agents":[{"id":"agent_5e6f7a8b","name":"后端","role":"owns the API","instructions":"run the tests","lastRunAt":null,"lastStatus":null,"lastSummary":null,"lastCreated":null,"lastCompleted":null}],"messages":[{"id":"msg_0123abcd","senderKind":"user","senderId":null,"senderName":"我","content":"让「研发部」按部门简介开工。","createdAt":"..."}],"tasks":{"total":0,"pending":0,"inProgress":0,"completed":0,"ready":0},"shift":null}]}}
```

`departments.create` takes `{"name","description","instructions","parent_id"}`;
`description`, `instructions`, and `parent_id` may be omitted. It answers
`{"department": {...}}` in the shape `departments.list` uses. A parent id the
workspace does not hold is `department_unknown`, and a malformed field is
`workforce_invalid`.

`departments.remove` takes `{"id"}` and answers `{"removed": [<department ids>]}`.
It removes the department and its descendants; task files stay on disk. It
refuses while any shift is running (`workforce_conflict`), because a shift writes
its report back to the department file.

`agents.add` takes `{"department_id","name","role","instructions"}` and answers
`{"department": {...}, "agent": {...}}`; `agents.remove` takes
`{"department_id","agent_id"}` and answers `{"department": {...}}`. Both refuse
an unknown department (`department_unknown`) or agent (`agent_unknown`) and both
refuse while that department is working (`workforce_conflict`).

`departments.run` takes `{"id","instruction"}` and answers
`{"accepted": true, "department": {...}}` as soon as the shift is reserved; the
agents work in the background. Progress arrives as `department.shift` and
`department.agent` events, and each member's final report is appended to the
department's group chat. A second run while one is active is
`workforce_conflict`; `departments.stop` takes `{"id"}` and answers
`{"stopped": true|false}`. Neither takes the one-turn slot used by `chat.send`.

`skills.list` takes no parameters and answers with the directory it scanned and
every skill in it:

```json
{"protocol_version":1,"type":"response","request_id":"host-14","ok":true,"result":{"dir":"D:\\work\\skills","count":1,"skills":[{"name":"demo","description":"A demo skill.","path":"D:\\work\\skills\\demo\\SKILL.md"}]}}
```

`skills.read` takes `{"name"}` and answers with the same fields plus `content`,
the whole manifest. A name the loader does not hold is `skill_unknown`, and the
message lists the names that would work.

`skills.import` takes `{"path"}` -- a folder holding a `SKILL.md`, or a folder of
such folders -- and answers with the directory it copied into and the skills now
there:

```json
{"protocol_version":1,"type":"request","request_id":"host-16","method":"skills.import","params":{"path":"C:\\Users\\me\\Downloads\\guizang-ppt-skill"}}
```
```json
{"protocol_version":1,"type":"response","request_id":"host-16","ok":true,"result":{"dir":"D:\\work\\skills","count":1,"skills":[{"name":"guizang-ppt-skill","description":"...","path":"D:\\work\\skills\\guizang-ppt-skill\\SKILL.md"}]}}
```

It skips `.git` as it copies, and it refuses rather than merges: a name already
in the workspace is `skill_exists`, a folder with no manifest anywhere under it
is `skill_invalid`, and a path that is not a folder is `skill_source`.

`plugins.list` takes no parameters and answers with every server this build ships
or the workspace declares. `connected` is the names connected so far; each entry
in `servers` carries `connected`, the `tools` discovered from it (empty until it
is connected), `source` (`builtin` or `workspace`), and `command` (the line that
will run, for a declared server, else `null`):

```json
{"protocol_version":1,"type":"response","request_id":"host-15","ok":true,"result":{"available":2,"connected":["docs"],"servers":[{"name":"docs","connected":true,"source":"builtin","command":null,"tools":[{"name":"mcp__docs__search","description":"Search the docs.","policy":"allow"}]},{"name":"notes","connected":false,"source":"workspace","command":"node tools/notes-mcp.mjs","tools":[]}]}}
```

`plugins.connect` takes `{"server"}` and answers `{"server","message"}`; a name
this build does not ship and the workspace does not declare is `plugin_unknown`,
and a known one whose catalogue cannot be built is `plugin_failed`. Connecting an
already-connected server is not an error: the answer says it was already
connected.

Request error codes: `invalid_json`, `invalid_utf8`, `invalid_request`,
`invalid_params`, `message_too_large`, `unknown_method`, `session_unknown`,
`session_store` (the conversations could not be read or written where they are
kept), `cron_invalid` (the expression is not one this runtime takes),
`cron_unknown` (no job with that id), `task_store` (the board could not be read
or written), `task_unknown` (no task, or no prerequisite, with that id),
`task_invalid` (a task field this runtime will not take), `task_blocked` (a
prerequisite is still open), `task_not_ready` (the task is not in the state the
action needs), `task_not_owned` (the task is owned by somebody other than the
page's `user`), `task_conflict` (another writer changed it under the action),
`skill_unknown` (no skill by that name),
`skill_source` (the path to import from is not a folder this runtime can read),
`skill_invalid` (no `SKILL.md` in it or any folder under it), `skill_exists` (a
skill of that name is already in the workspace), `plugin_unknown` (not a server
this build ships or the workspace declares), `plugin_failed` (a known one would
not connect), `unsupported_protocol_version`. A turn that
failed reports its own code on
`chat.failed`, so the page can branch on it: `chat_busy`,
`chat_not_configured`, `chat_unauthorized`, `chat_rate_limited`,
`chat_api_error`, `chat_step_limit`, `chat_recovery_failed`, `chat_turn_failed`,
`chat_interrupted`. Only one turn runs at a time: a second prompt sent while one
is in flight is refused with `chat_busy` rather than queued.

Framing rules the host enforces: a response with an unknown `request_id` is
logged, not applied; every in-flight request fails with a reason when the child
exits or the connection drops; `runtime.ready` is what makes a start "connected"
rather than merely "spawned".

Exit codes of `desktop_bridge`: `0` clean (including stdin EOF), `1` I/O error,
`2` usage error, `3` workspace error, `4` the conversations' directory could not
be used. Closing stdin is a valid stop signal, so an orphaned child exits on its
own even if the host never asks politely.

## Lifecycle

- The host starts the runtime during window setup. The page can only observe
  and ask; it never gets a handle to a process.
- `start` is idempotent while the runtime is up, and "running" means
  `runtime.ready` arrived, not that a process was spawned.
- Closing the window is an ordered shutdown: the host refuses the exit once,
  stops the runtime on the async runtime, then exits for real. Doing that work
  in `RunEvent::Exit` would block the event-loop thread on the `emit`s that
  stopping produces, which deadlocks and leaves the runtime running.
- A killed runtime is not papered over: every in-flight request fails, the
  status becomes `error`, the snapshot is cleared, and the UI offers 启动 again.
- Dropping the host's stdin pipe is itself a stop signal, so even a hard kill of
  the host does not leave a runtime behind.

## Testing

```bash
cargo test --all-targets                      # runtime + bridge
node scripts/desktop-bridge-smoke.mjs         # drives the real bridge binary
cd desktop && pnpm typecheck && pnpm build
cd desktop/src-tauri && cargo test --all-targets
```

Measured on Windows 11, `x86_64-pc-windows-msvc`, 2026-10-14:

- `cargo test --all-targets`: 202 passed / 0 failed, plus 43 for the bridge.
- `desktop/src-tauri` `cargo test --all-targets`: 43 passed / 0 failed.
- `node scripts/desktop-bridge-smoke.mjs`: 116 of 116 checks passed against the
  built binary (framing, ids, errors, stdout purity, clean shutdown, the
  workspace `.env` precedence, and a full chat turn driven against a local HTTP
  stand-in for the model, read back afterwards through `chat.history`; a turn
  stopped mid-flight and the prompt that follows it; and a conversation left on
  disk and picked up by a second process, with a 新会话 listed as the one being
  continued before it has anything to write, renamed and then deleted; and a
  bridge given its own `--sessions-dir`, which leaves the workspace without a
  `.sessions` directory at all).
- `pnpm typecheck` and `pnpm build`: clean.
- `pnpm app:build`: builds both bundles, and leaves a runnable layout in
  `src-tauri/target/release/` -- the app next to its sidecar, so it can be run
  without installing anything.

Checked by hand in a real window against a real child process:

- `pnpm app:dev` opens the window, the host starts the runtime, and the page
  reports `Ready` with the pid, uptime, workspace, and paths the runtime itself
  reported.
- 连接检查 round-trips and shows `最近一次连接检查成功 · 0 ms · <time>`.
- Killing the bridge shows `Error` / `runtime_exited` /
  "the runtime exited unexpectedly (exit code 1)", clears the snapshot, and
  re-enables 启动 Runtime.
- Closing the window ends the app and the bridge within a second, leaving
  nothing behind.
- The release binary runs standalone off the sidecar beside it, and uses the
  workspace under the per-user app data directory.

The project column and the folder picker came after that pass and have not been
exercised in a real window yet. What is covered: `projects.rs` unit tests,
`cargo check --all-targets`, `pnpm typecheck`, and `pnpm build`. Still to check
by hand: opening the picker, adding and removing a project, and watching a live
runtime move workspace when another project is chosen.

The Settings form came after that pass too. What is covered: `provider.rs` unit
tests (the store, its validation, the write-only key, and the environment it
produces), `pnpm typecheck`, `pnpm build`, and the smoke test for the workspace
`.env` precedence. Still to check by hand: typing into the form, saving a key,
clearing it, and watching a live runtime restart into the new environment.

The Chat page and the `chat.send` method came after that pass as well. What is
covered: the bridge's `chat.*` unit tests, a full turn in the smoke test against
a local HTTP stand-in for the model (streamed deltas, a tool call, a failure
with a code, a prompt refused mid-turn, and the same turn read back through
`chat.history`), `pnpm typecheck`, and `pnpm build`. Still to check by hand:
typing a prompt into a real window and watching it stream, reloading the window
mid-answer and watching the transcript come back, and the Settings-key flow that
makes a prompt runnable at all.

Stopping a turn and keeping conversations on disk came after that pass as well.
What is covered: the bridge's `TurnCancel` and session unit tests, a stop driven
through the smoke test against the local HTTP stand-in (a stop aimed at nothing,
a stop that reaches a turn, the prompt that follows it, and the stopped turn read
back as `cancelled`), the same conversation re-opened by a second process,
`pnpm typecheck`, and `pnpm build`. Still to check by hand: pressing 停止 in a
real window, watching 新会话 and the conversation picker swap transcripts, and
seeing a reload come back to a conversation a restart no longer loses.

Renaming and deleting conversations, and keeping them outside the workspace, came
after that pass too. What is covered: `session.rs` unit tests (a rename that
rewrites the title and nothing else), the bridge's `sessions.*` unit tests (an
in-memory name for a conversation with no file yet, deleting the conversation in
hand, deleting the last one, and every refusal), `paths.rs` unit tests for the
store key, the smoke test's rename and delete round trips plus a run with
`--sessions-dir`, `pnpm typecheck`, and `pnpm build`. Still to check by hand:
renaming and deleting from the picker in a real window, and a project whose
conversations now live under the app data directory rather than in the folder.

Scheduling tasks from the 定时任务 page came after that pass as well. What is
covered: `cron.rs` unit tests for `next_match` (the strictly-next minute, a step
expression, the weekday rule, and a date that can never match); the bridge's
`cron.*` unit tests (a schedule/list/cancel round trip, every refusal and its
code, the `[Scheduled]` line a due job owes, and the methods taking only what
they document); the smoke test's cron round trip and a due job the ticker marks
while nothing is running; `cargo check --all-targets`, `pnpm typecheck`, and
`pnpm build`. Still to check by hand: adding a job in a real window, watching it
fire while the app sits idle, stopping the turn it started, and removing it.

The 技能 and 插件 pages came after that too, and they share one property worth
stating: neither invents a capability the runtime does not have. What is covered:
the bridge's `skills.*` unit tests (a list/read round trip, the path of the
manifest that was read, an import that lands and one that is refused with a code,
every refusal and its code) and `plugins.*` unit tests (a list/connect round trip,
a catalogue that is empty until connected and namespaced once it is, the host
policy on each tool, a workspace that declares its own server, and every
refusal); `chat.context`'s read-back of the size the compactor measures; the
smoke test's catalogue read, a skill import and its refusals, a plugin connect, a
workspace-declared stdio plugin connected against a real child process, and the
status counts that follow; `cargo test --bin desktop_bridge`, `pnpm typecheck`,
and `pnpm build`. Still to check by hand: importing a skill and connecting a
plugin in a real window, then asking the model in the chat to load the skill or
use one of its `mcp__*` tools.

The 任务 page came after that. What is covered: the bridge's `tasks.*` unit tests
(a create/list round trip, a node behind a prerequisite, a blocked claim, a
claim/release/complete cycle that reports the node it unblocked, every refusal
and its code, and the methods taking only what they document); the smoke test's
create/claim/release/complete round trip against the real binary and the status
counts that follow it; `cargo test --all-targets` in both crates, `pnpm
typecheck`, and `pnpm build`. Still to check by hand: adding a node in a real
window, picking prerequisites, and watching the model's `create_task` output
appear on the same board.

## Platform status

| platform | build | runtime round trip |
| --- | --- | --- |
| Windows 11 x64 (`x86_64-pc-windows-msvc`) | yes, MSI produced | verified |
| macOS / Linux | **not verified** | **not verified** |

What is cross-platform by construction, and what is not:

- `RuntimeManager` uses only `tokio::process`, `std::process`, and `AppHandle`:
  no Windows-specific API, no shell.
- Paths and the executable suffix are decided in `paths.rs` from `cfg!(windows)`; macOS additionally looks in `Contents/Resources` beside the
  executable.
- `scripts/build-bridge.mjs` stages the **host** triple only. Building for
  another platform means running it on that platform (or cross-compiling and
  staging the artifact by hand): a Windows binary copied under a macOS triple
  name would be a lie the bundler cannot catch.
- `tauri.conf.json` carries macOS (`minimumSystemVersion`) and Linux (`deb`)
  sections so the bundler has somewhere to go, but neither has been exercised.
- Linux needs `webkit2gtk-4.1`, `libayatana-appindicator3-dev`, and
  `librsvg2-dev` installed before `pnpm app:dev` will link.
- Windows-specific work still to do: none known.
- `pnpm app:build` produced both bundles: the MSI (2.2 MiB) and the NSIS setup
  (1.5 MiB). An earlier run produced only the MSI because the NSIS toolchain
  download timed out -- a network condition, not a build failure. If it
  recurs, `pnpm tauri build --bundles msi` builds the MSI alone.

## Not in this phase

The chat runs one turn at a time, and what it does not do is deliberate: a prompt
sent while another is running is refused rather than queued (only stopping frees
the slot), and a turn's after-work (the debounced memory pass) is not streamed
back. A stopped turn leaves nothing behind on disk, because half an exchange is
not a turn anybody had.

The schedule has no pause: a job is there or it is cancelled. A job that comes
due while a turn is running waits for the one-turn slot rather than pre-empting
it, and a turn the runtime started on its own is a turn like any other -- the
only trace of where it came from is the `[Scheduled]` line the model received.

Skills can be added from the page but not edited: `skills.import` copies a folder
in, and there is nothing to toggle after that, since the loader scans a directory
and the runtime keeps no per-skill state. A skill is removed by deleting its
directory, which the page does not do, because the runtime cannot tell a skill it
was handed from any other file. The plugins only grow: a connected server cannot
be disconnected without restarting the runtime, and the page says so rather than
pretending the button works. Neither page takes the one-turn slot, so importing a
skill or connecting a plugin while the model is answering costs nothing.

Departments now cover creation, sub-departments, agents, group chat, sequential
shifts, and agents that create, claim, and complete their own department-scoped
tasks. Two pieces are still deliberately absent: the 任务 page cannot hand a
specific task to a named agent, and there is no acceptance/review step after an
agent reports completion -- the owner id and the task board are the record. The
protocol grows by adding methods, not by changing the framing.

The per-binary harness assembly (workspace + stores) is duplicated between
`src/main.rs`, `src/bin/tui.rs`, and `src/bin/desktop_bridge.rs`. Lifting it
into `rust_agent::harness` is the obvious second-phase cleanup; it was left
alone here to keep the runtime's own build untouched.
