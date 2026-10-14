# rust-agent desktop workbench

A Tauri 2 + Vue 3 shell that hosts the `rust-agent` runtime as a child process
and talks to it over JSON Lines on stdin/stdout.

**Phase 1 scope.** This is infrastructure, not product. It proves the shell can
start the real runtime, correlate requests, stream events, show honest status,
and shut down without leaving orphans. It also ships the shell's chrome: an icon
rail of destinations, a project column backed by a real folder picker, and a
chat page that says out loud that the protocol has no prompt method yet.
Departments, group chat, agents, and tasks are not implemented; the pages that
will hold them are `待建` placeholders on purpose.

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
| rail (`src/components/AppRail.vue`) | 定时任务 / 插件 / 技能, the app mark, the runtime's health dot, and 设置 |
| projects (`src/components/ProjectPanel.vue`) | 添加项目 (the system folder picker), the stored projects, which one is active, and a runtime state badge |
| work (`src/views/`) | the routed page: 对话, the `待建` page behind a rail destination, or 设置 |

The rail lists only destinations that exist. The three that are not built are
pages, not dead links, and they carry the facts `runtime.status` already reports
about their area -- how many cron jobs, which skills are loaded -- rather than
invented ones.

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

The conversation is also written down as it goes, under `<workspace>/.sessions`,
so it outlives the process that held it. Restarting the runtime -- or choosing
another project, which restarts it -- does not end it: the runtime continues the
conversation it was last holding, and a page that reloads reads it back the way
it reads back a reload. The conversation picker on the chat page lists what this
workspace holds, marks the one being continued, and can start a fresh one or
pick up an older one.

- The agent is built on the first prompt, not at startup, so a runtime with no
  model configured still starts and still answers for itself. A prompt sent
  before one is configured is refused with `chat_not_configured`, before it
  takes the one-turn slot, so a refusal never blocks a later prompt.
- A turn is non-interactive. There is no terminal behind the window, so a call
  the permission policy would have asked about is denied with a reason and
  shown as `chat.blocked` rather than blocking on an approval nobody can give.
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
`src/lib/provider/index.ts`, `src/lib/chat/index.ts`) are the only modules that
import `@tauri-apps/api`, and `src/lib/shell.ts` holds the one guard they
share. The runtime seam exposes `getRuntimeStatus`,
`startRuntime`, `stopRuntime`, `restartRuntime`, `pingRuntime`,
`getRuntimeLogs`, `getHostInfo`, `subscribeRuntimeEvents`, and
`unsubscribeRuntimeEvents`. The projects seam exposes `listProjects`,
`pickProject`, `selectProject`, and `removeProject`. The provider seam exposes
`getProviderSettings` and `saveProviderSettings`. The chat seam exposes
`sendChatPrompt`, `cancelChatTurn`, `fetchChatHistory`, `fetchChatSessions`,
`newChatSession`, `openChatSession`, and `subscribeChatEvents`; a prompt is the
one thing the page chooses there, and the answer arrives on the events rather
than the command.

**Host commands** (`src-tauri/src/commands.rs`) are thin wrappers:
`runtime_status`, `runtime_start`, `runtime_stop`, `runtime_restart`,
`runtime_ping`, `runtime_logs`, `runtime_host_info`, `projects_list`,
`projects_pick`, `projects_select`, `projects_remove`, `provider_get`,
`provider_save`, `chat_send`, `chat_cancel`, `chat_history`, `chat_sessions`,
`chat_new_session`, and `chat_open_session`. None of them takes a path, a
binary, or a command line from the caller, and the commands that name something
the runtime holds -- a project, a conversation -- take an id the runtime itself
handed out. `provider_save` is the one command that takes a secret in -- and its
answer still does not contain one. `chat_send` takes the prompt text and nothing
else; the workspace, the model, and the credentials are all decided in the host.
`chat_open_session` takes a session id, and only one this workspace holds: the
runtime checks its shape before it touches the disk, and answers
`session_unknown` for an id that is well formed but not there.

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

## Protocol (v1)

One JSON object per line, UTF-8, `\n`-framed. **stdout carries protocol only**;
every diagnostic goes to stderr as `{"ts","level","target","message"}`. A line
over 1 MiB is dropped and the stream resynchronises at the next newline.

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
`chat.send`, `chat.cancel`, `chat.history`, `sessions.list`, `sessions.new`, and
`sessions.open`. Only `chat.send` and `sessions.open` take parameters -- the
others refuse any key at all, so a host that thinks it is talking to a newer
bridge finds out at once. `chat.send`'s request carries the prompt and nothing
else:

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
`chat.tool_result`, `chat.blocked` (a permission-sensitive call refused because
there is no terminal to approve it), `chat.notice` (progress that is not an
answer -- compaction, memory, recovery, cron), and finally exactly one of
`chat.done` (with the whole answer), `chat.failed` (with an `error`), or
`chat.cancelled` (the turn was stopped).

`chat.history` takes no parameters and answers with the turns reported so far,
oldest first. Each one carries the prompt as it was typed, its status (`running`,
`done`, `failed` or `cancelled`), the code and the provider's words when it
failed, and the very events it arrived as -- so a page can render a conversation
it did not watch. The log is bounded: oldest turns go first once it outgrows 50
turns or 4 MiB, and a turn that is still running is never dropped.

`chat.cancel` takes no parameters and answers with whether a turn was stopped. A
stop that finds nothing running answers `{"cancelled":false}` on the spot; a stop
that reaches a turn is answered by that turn, once it has stopped and the slot it
held is free. That is what makes the two usable in sequence: the prompt the page
sends on the back of the answer is taken, rather than refused as a turn still
running.

Conversations are written to `<workspace>/.sessions/<id>.json`, and the runtime
continues the newest one it finds when it starts. `sessions.list` takes no
parameters and answers with what this workspace holds, newest first, plus the id
being continued:

```json
{"protocol_version":1,"type":"response","request_id":"host-11","ok":true,"result":{"current":"s1750000000000-4821-0","sessions":[{"id":"s1750000000000-4821-0","title":"say hello","created_at":"2026-10-14T05:12:00.123456700+00:00","updated_at":"2026-10-14T05:14:31.765432100+00:00","messages":6}]}}
```

`sessions.new` takes no parameters and starts a fresh conversation, leaving the
old one on disk; `sessions.open` takes the id of one this workspace holds and
continues it, numbering its turns after the ones it already has. Both refuse
while a turn is running. A turn the file caught mid-flight -- the process was
killed, not stopped -- reads back as `failed` with `chat_interrupted` rather than
left spinning, because whatever was going to finish it is gone.

Request error codes: `invalid_json`, `invalid_utf8`, `invalid_request`,
`invalid_params`, `message_too_large`, `unknown_method`, `session_unknown`,
`unsupported_protocol_version`. A turn that failed reports its own code on
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
`2` usage error, `3` workspace error. Closing stdin is a valid stop signal, so
an orphaned child exits on its own even if the host never asks politely.

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

- `cargo test --all-targets`: 201 passed / 0 failed, plus 35 for the bridge.
- `desktop/src-tauri` `cargo test --all-targets`: 41 passed / 0 failed.
- `node scripts/desktop-bridge-smoke.mjs`: 92 of 92 checks passed against the
  built binary (framing, ids, errors, stdout purity, clean shutdown, the
  workspace `.env` precedence, and a full chat turn driven against a local HTTP
  stand-in for the model, read back afterwards through `chat.history`; a turn
  stopped mid-flight and the prompt that follows it; and a conversation left on
  disk and picked up by a second process).
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
the slot), a conversation cannot be renamed or deleted from the page, and a
turn's after-work (the debounced memory pass) is not streamed back. A stopped
turn leaves nothing behind on disk, because half an exchange is not a turn
anybody had.

Departments, agents, group chat, task claiming, delivery, and acceptance are
next. `runtime.status` already reports skills, tasks, memory, and cron counts,
so those pages have data to build on; the protocol grows by adding methods, not
by changing the framing.

The per-binary harness assembly (workspace + stores) is duplicated between
`src/main.rs`, `src/bin/tui.rs`, and `src/bin/desktop_bridge.rs`. Lifting it
into `rust_agent::harness` is the obvious second-phase cleanup; it was left
alone here to keep the runtime's own build untouched.
