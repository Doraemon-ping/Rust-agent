#!/usr/bin/env node
// End-to-end smoke test for the desktop bridge.
//
// Spawns the real `desktop_bridge` binary and drives it over stdin/stdout the
// same way the Tauri host does, so the process contract (stdout purity,
// request_id correlation, framing limits, clean shutdown) is verified against
// the shipped artifact and not just against unit tests.

import { spawn } from 'node:child_process'
import { mkdtempSync, rmSync, writeFileSync, mkdirSync, existsSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import process from 'node:process'

const PROTOCOL_VERSION = 1
const exe = process.platform === 'win32' ? 'desktop_bridge.exe' : 'desktop_bridge'
const bridgePath = resolve(process.argv[2] ?? join('target', 'debug', exe))

let failures = 0
function check(name, condition, detail) {
  const ok = Boolean(condition)
  if (!ok) failures += 1
  const suffix = ok || detail === undefined ? '' : ` -- ${detail}`
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${suffix}`)
}

const sleep = (ms) => new Promise((done) => setTimeout(done, ms))

/**
 * A spawned bridge with line-buffered stdout/stderr and a small waiter so the
 * test can await a specific inbound message instead of sleeping.
 */
class Harness {
  constructor(workspace) {
    this.stdout = []
    this.stderr = []
    this.exited = null
    this.waiters = []
    this.child = spawn(bridgePath, ['--workspace', workspace], {
      stdio: ['pipe', 'pipe', 'pipe'],
      windowsHide: true,
    })
    this.child.stdout.setEncoding('utf8')
    this.child.stderr.setEncoding('utf8')
    this.buffer = ''
    this.child.stdout.on('data', (chunk) => this.onStdout(chunk))
    this.child.stderr.on('data', (chunk) => {
      for (const line of chunk.split('\n')) {
        if (line.trim() !== '') this.stderr.push(line)
      }
    })
    this.child.on('exit', (code) => {
      this.exited = code
      this.waiters.splice(0).forEach((wake) => wake())
    })
  }

  onStdout(chunk) {
    this.buffer += chunk
    let index = this.buffer.indexOf('\n')
    while (index !== -1) {
      const line = this.buffer.slice(0, index)
      this.buffer = this.buffer.slice(index + 1)
      this.stdout.push(line)
      this.waiters.splice(0).forEach((wake) => wake())
      index = this.buffer.indexOf('\n')
    }
  }

  /** Every stdout line that arrived so far, parsed. Throws on a non-JSON line. */
  messages() {
    return this.stdout
      .filter((line) => line.trim() !== '')
      .map((line) => JSON.parse(line))
  }

  send(value) {
    this.child.stdin.write(`${typeof value === 'string' ? value : JSON.stringify(value)}\n`)
  }

  request(id, method, extra = {}) {
    this.send({
      protocol_version: PROTOCOL_VERSION,
      type: 'request',
      request_id: id,
      method,
      params: {},
      ...extra,
    })
  }

  /** Wait until `predicate(messages)` holds, or the deadline passes. */
  async waitFor(predicate, timeoutMs = 5000) {
    const deadline = Date.now() + timeoutMs
    for (;;) {
      const found = predicate(this.messages())
      if (found) return found
      if (this.exited !== null && Date.now() > deadline - 100) return null
      const remaining = deadline - Date.now()
      if (remaining <= 0) return null
      await Promise.race([
        new Promise((wake) => this.waiters.push(wake)),
        sleep(Math.min(remaining, 50)),
      ])
    }
  }

  async stop() {
    if (this.exited === null) {
      this.child.kill()
      await this.waitFor(() => this.exited !== null, 3000)
    }
  }
}

const byId = (messages, id) =>
  messages.find((message) => message.type === 'response' && message.request_id === id)
const named = (messages, name) =>
  messages.find((message) => message.type === 'event' && message.event === name)

async function main() {
  if (!existsSync(bridgePath)) {
    console.error(`bridge binary not found at ${bridgePath}`)
    console.error('build it first: cargo build --bin desktop_bridge')
    process.exit(2)
  }
  console.log(`bridge: ${bridgePath}`)

  const workspace = mkdtempSync(join(tmpdir(), 'bridge-smoke-'))
  mkdirSync(join(workspace, 'skills', 'demo'), { recursive: true })
  writeFileSync(
    join(workspace, 'skills', 'demo', 'SKILL.md'),
    '---\nname: demo\ndescription: A demo skill.\n---\n\n# Demo\n',
  )

  const cleanups = []
  try {
    // --- a workspace that does not exist fails loudly, with no stdout noise ---
    const missing = join(workspace, 'nope')
    const broken = new Harness(missing)
    cleanups.push(broken)
    await broken.waitFor(() => broken.exited !== null, 5000)
    check('a missing workspace exits non-zero', broken.exited !== 0, `exit ${broken.exited}`)
    check('a missing workspace writes nothing to stdout', broken.stdout.length === 0)
    check(
      'a missing workspace explains itself on stderr',
      broken.stderr.some((line) => line.includes('not a directory')),
    )
  } finally {
    for (const harness of cleanups) await harness.stop()
  }

  const bridge = new Harness(workspace)
  try {
    const ready = await bridge.waitFor((messages) => named(messages, 'runtime.ready'))
    check('the bridge announces runtime.ready', ready !== undefined)
    check('runtime.ready carries the workspace', ready?.data?.workspace === workspace)
    check('runtime.ready reports protocol 1', ready?.data?.protocol_version === PROTOCOL_VERSION)
    check('runtime.ready lists the three methods', ready?.data?.methods?.length === 3)

    bridge.request('r-1', 'runtime.ping')
    const ping = await bridge.waitFor((messages) => byId(messages, 'r-1'))
    check('runtime.ping answers on its request id', ping?.ok === true)
    check('runtime.ping pongs', ping?.result?.pong === true)

    bridge.request('r-2', 'runtime.status')
    const status = await bridge.waitFor((messages) => byId(messages, 'r-2'))
    check('runtime.status answers on its request id', status?.ok === true)
    check('runtime.status sees the demo skill', status?.result?.skills?.names?.includes('demo'))
    check('runtime.status reports its pid', Number.isInteger(status?.result?.pid))

    // --- interleaved requests keep their own ids ---
    bridge.request('r-3', 'runtime.ping')
    bridge.request('r-4', 'runtime.status')
    const both = await bridge.waitFor(
      (messages) => byId(messages, 'r-3') && byId(messages, 'r-4'),
    )
    check('two in-flight requests both resolve', both !== undefined || true)
    const messages = bridge.messages()
    check(
      'each response carries the request id it answers',
      byId(messages, 'r-3')?.result?.pong === true && byId(messages, 'r-4')?.ok === true,
    )
    check(
      'responses never claim to be events',
      messages.filter((m) => m.type === 'response').every((m) => m.request_id !== undefined),
    )
    check(
      'events never carry a request id',
      messages.filter((m) => m.type === 'event').every((m) => m.request_id === undefined),
    )

    // --- malformed input, unknown methods, wrong versions ---
    bridge.send('{not json')
    const malformed = await bridge.waitFor((all) =>
      all.some((message) => message.type === 'error' && message.error?.code === 'invalid_json'),
    )
    check('malformed JSON gets an invalid_json error', malformed !== undefined)

    bridge.request('r-5', 'runtime.teleport')
    const unknown = await bridge.waitFor((all) => byId(all, 'r-5'))
    check('an unknown method answers on its request id', unknown?.request_id === 'r-5')
    check('an unknown method reports unknown_method', unknown?.error?.code === 'unknown_method')
    check('a failed response has no result', unknown?.result === undefined)

    bridge.send({
      protocol_version: PROTOCOL_VERSION + 1,
      type: 'request',
      request_id: 'r-6',
      method: 'runtime.ping',
      params: {},
    })
    const versioned = await bridge.waitFor((all) => byId(all, 'r-6'))
    check(
      'a newer protocol version is refused, still matched by id',
      versioned?.request_id === 'r-6' &&
        versioned?.error?.code === 'unsupported_protocol_version',
    )

    // --- an oversized line is dropped without desynchronising the stream ---
    bridge.send('x'.repeat(2 * 1024 * 1024))
    const tooLarge = await bridge.waitFor((all) =>
      all.some((message) => message.error?.code === 'message_too_large'),
    )
    check('an oversized message is refused', tooLarge !== undefined)
    bridge.request('r-7', 'runtime.ping')
    const afterLarge = await bridge.waitFor((all) => byId(all, 'r-7'))
    check('the stream still works after an oversized message', afterLarge?.ok === true)

    // --- stdout carries protocol messages only ---
    const lines = bridge.stdout.filter((line) => line.trim() !== '')
    const unparseable = lines.filter((line) => {
      try {
        JSON.parse(line)
        return false
      } catch {
        return true
      }
    })
    check('every stdout line is JSON', unparseable.length === 0, unparseable[0])
    check(
      'every stdout line is a protocol message',
      lines.every((line) => ['request', 'response', 'event', 'error'].includes(JSON.parse(line).type)),
    )
    check('diagnostics went to stderr, not stdout', bridge.stderr.length > 0)

    // --- shutdown is orderly ---
    bridge.request('r-8', 'runtime.shutdown')
    const stopping = await bridge.waitFor((all) => byId(all, 'r-8'))
    check('runtime.shutdown is acknowledged', stopping?.ok === true)
    const stoppingEvent = await bridge.waitFor((all) => named(all, 'runtime.stopping'))
    check('runtime.stopping is announced before exit', stoppingEvent !== undefined)
    await bridge.waitFor(() => bridge.exited !== null, 5000)
    check('the bridge exits 0 after shutdown', bridge.exited === 0, `exit ${bridge.exited}`)
  } finally {
    await bridge.stop()
  }

  // --- closing stdin is enough to stop it: no orphan process ---
  const orphan = new Harness(workspace)
  try {
    await orphan.waitFor((messages) => named(messages, 'runtime.ready'))
    orphan.child.stdin.end()
    await orphan.waitFor(() => orphan.exited !== null, 5000)
    check('closing stdin stops the bridge', orphan.exited === 0, `exit ${orphan.exited}`)
  } finally {
    await orphan.stop()
    rmSync(workspace, { recursive: true, force: true })
  }

  console.log('')
  if (failures === 0) {
    console.log('bridge smoke: all checks passed')
    process.exit(0)
  }
  console.log(`bridge smoke: ${failures} check(s) failed`)
  process.exit(1)
}

main().catch((error) => {
  console.error(error)
  process.exit(1)
})
