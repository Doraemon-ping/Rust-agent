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

## How the pieces fit

```
Vue view  ->  Pinia store  ->  lib/runtime + lib/projects (the two seams)
          ->  Tauri command (commands.rs)
          ->  RuntimeManager (runtime.rs)  ->  child process stdin
child stdout  ->  reader task  ->  status + events  ->  emit  ->  store  ->  view
```

**Frontend services** (`src/lib/runtime/index.ts`, `src/lib/projects/index.ts`)
are the only modules that import `@tauri-apps/api`, and `src/lib/shell.ts` holds
the one guard they share. The runtime seam exposes `getRuntimeStatus`,
`startRuntime`, `stopRuntime`, `restartRuntime`, `pingRuntime`,
`getRuntimeLogs`, `getHostInfo`, `subscribeRuntimeEvents`, and
`unsubscribeRuntimeEvents`. The projects seam exposes `listProjects`,
`pickProject`, `selectProject`, and `removeProject`.

**Host commands** (`src-tauri/src/commands.rs`) are thin wrappers:
`runtime_status`, `runtime_start`, `runtime_stop`, `runtime_restart`,
`runtime_ping`, `runtime_logs`, `runtime_host_info`, `projects_list`,
`projects_pick`, `projects_select`, `projects_remove`. None of them takes a
path, a binary, or a command line from the caller, and the two projects commands
that name a project take an id the host itself handed out.

**Host events** are `runtime://status`, `runtime://message`, and `runtime://log`.
Status is pushed on every state change; the page never polls and never infers.

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

Methods in phase 1: `runtime.status` (a full snapshot), `runtime.ping`, and
`runtime.shutdown`. Error codes: `invalid_json`, `invalid_utf8`,
`invalid_request`, `invalid_params`, `message_too_large`, `unknown_method`,
`unsupported_protocol_version`.

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

- `cargo test --all-targets`: 191 passed / 0 failed, plus 15 for the bridge.
- `desktop/src-tauri` `cargo test --all-targets`: 29 passed / 0 failed.
- `node scripts/desktop-bridge-smoke.mjs`: 30 of 30 checks passed against the
  built binary (framing, ids, errors, stdout purity, clean shutdown).
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

The chat page is a placeholder, and honestly so: protocol v1 answers
`runtime.status`, `runtime.ping`, and `runtime.shutdown`, and nothing takes a
prompt. So the page lists those methods -- read from the runtime's own snapshot,
not hard-coded -- and shows a disabled composer instead of a fake conversation.
A real chat is a bridge method that accepts a prompt and streams the model's
answer back as events.

Departments, agents, group chat, task claiming, delivery, and acceptance are
next. `runtime.status` already reports skills, tasks, memory, and cron counts,
so those pages have data to build on; the protocol grows by adding methods, not
by changing the framing.

The per-binary harness assembly (workspace + stores) is duplicated between
`src/main.rs`, `src/bin/tui.rs`, and `src/bin/desktop_bridge.rs`. Lifting it
into `rust_agent::harness` is the obvious second-phase cleanup; it was left
alone here to keep the runtime's own build untouched.
