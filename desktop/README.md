# rust-agent desktop workbench

A Tauri 2 + Vue 3 shell that hosts the `rust-agent` runtime as a child process
and talks to it over JSON Lines on stdin/stdout.

**Phase 1 scope.** This is infrastructure, not product. It proves the shell can
start the real runtime, correlate requests, stream events, show honest status,
and shut down without leaving orphans. Departments, agents, group chat, and
tasks are not implemented; the navigation lists them as `待建` placeholders on
purpose.

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

## How the pieces fit

```
Vue view  ->  Pinia store  ->  lib/runtime (the only @tauri-apps/api importer)
          ->  Tauri command (commands.rs)
          ->  RuntimeManager (runtime.rs)  ->  child process stdin
child stdout  ->  reader task  ->  status + events  ->  emit  ->  store  ->  view
```

**Frontend service** (`src/lib/runtime/index.ts`) is the single seam. It exposes
`getRuntimeStatus`, `startRuntime`, `stopRuntime`, `restartRuntime`,
`pingRuntime`, `getRuntimeLogs`, `getHostInfo`, `subscribeRuntimeEvents`, and
`unsubscribeRuntimeEvents`, and it is the only file that imports
`@tauri-apps/api`.

**Host commands** (`src-tauri/src/commands.rs`) are thin wrappers that take no
paths and no arguments from the caller: `runtime_status`, `runtime_start`,
`runtime_stop`, `runtime_restart`, `runtime_ping`, `runtime_logs`,
`runtime_host_info`.

**Host events** are `runtime://status`, `runtime://message`, and `runtime://log`.
Status is pushed on every state change; the page never polls and never infers.

**Where the binary comes from** (`src-tauri/src/paths.rs`) is decided in exactly
one place, in this order: `RUST_AGENT_BRIDGE` (debug builds only), the bundled
sidecar next to the executable, then the sibling cargo target directory so
`tauri dev` works without packaging. The frontend cannot name an executable or
pass it a command line. There is no shell capability in
`capabilities/default.json` beyond `core:event:default`.

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

- `cargo test --all-targets`: 179 passed / 0 failed, plus 14 for the bridge.
- `desktop/src-tauri` `cargo test --all-targets`: 20 passed / 0 failed.
- `node scripts/desktop-bridge-smoke.mjs`: 30 of 30 checks passed against the
  built binary (framing, ids, errors, stdout purity, clean shutdown).
- `pnpm typecheck` and `pnpm build`: clean.
- `pnpm app:build`: builds, and the MSI lands in
  `src-tauri/target/release/bundle/msi/`.

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
- Known gap in this environment: `targets: "all"` also asks for NSIS, and the
  NSIS toolchain download timed out here, so only the MSI was produced. That is
  a network condition, not a build failure; retry with a working connection or
  run `pnpm tauri build --bundles msi`.

## Not in this phase

Departments, agents, group chat, task claiming, delivery, and acceptance are
next. `runtime.status` already reports skills, tasks, memory, and cron counts,
so those pages have data to build on; the protocol grows by adding methods, not
by changing the framing.

The per-binary harness assembly (workspace + stores) is duplicated between
`src/main.rs`, `src/bin/tui.rs`, and `src/bin/desktop_bridge.rs`. Lifting it
into `rust_agent::harness` is the obvious second-phase cleanup; it was left
alone here to keep the runtime's own build untouched.
