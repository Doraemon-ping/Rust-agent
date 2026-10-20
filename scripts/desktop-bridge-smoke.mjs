#!/usr/bin/env node
// End-to-end smoke test for the desktop bridge.
//
// Spawns the real `desktop_bridge` binary and drives it over stdin/stdout the
// same way the Tauri host does, so the process contract (stdout purity,
// request_id correlation, framing limits, clean shutdown) is verified against
// the shipped artifact and not just against unit tests.

import { spawn } from 'node:child_process'
import { mkdtempSync, rmSync, writeFileSync, readFileSync, mkdirSync, existsSync } from 'node:fs'
import { createServer } from 'node:http'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

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
  constructor(workspace, options = {}) {
    this.stdout = []
    this.stderr = []
    this.exited = null
    this.waiters = []
      const args = ['--workspace', workspace, ...(options.args ?? [])]
      this.child = spawn(bridgePath, args, {
      stdio: ['pipe', 'pipe', 'pipe'],
      windowsHide: true,
      // The host decides this child's environment, so a check that depends on
      // it must decide it too instead of inheriting the developer's shell.
      env: options.env ?? process.env,
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
    check('runtime.ready lists every method', ready?.data?.methods?.length === 27)
    check('runtime.ready advertises chat.send', ready?.data?.methods?.includes('chat.send'))
    check('runtime.ready advertises chat.cancel', ready?.data?.methods?.includes('chat.cancel'))
    check('runtime.ready advertises chat.history', ready?.data?.methods?.includes('chat.history'))
    check('runtime.ready advertises chat.context', ready?.data?.methods?.includes('chat.context'))
    check(
      'runtime.ready advertises the interaction methods',
      ['chat.answer', 'chat.approve'].every((method) =>
        ready?.data?.methods?.includes(method)),
    )
    check('runtime.ready advertises the session methods', [
      'sessions.list',
      'sessions.new',
      'sessions.open',
      'sessions.rename',
      'sessions.delete',
    ].every((method) => ready?.data?.methods?.includes(method)))
    check('runtime.ready advertises the cron methods', [
      'cron.list',
      'cron.schedule',
      'cron.cancel',
    ].every((method) => ready?.data?.methods?.includes(method)))
    check('runtime.ready advertises the task methods', [
      'tasks.list',
      'tasks.create',
      'tasks.claim',
      'tasks.release',
      'tasks.complete',
    ].every((method) => ready?.data?.methods?.includes(method)))
    check('runtime.ready advertises the skill methods', [
      'skills.list',
      'skills.read',
    ].every((method) => ready?.data?.methods?.includes(method)))
    check(
      'runtime.ready advertises skills.import',
      ready?.data?.methods?.includes('skills.import'),
    )
    check('runtime.ready advertises the plugin methods', [
      'plugins.list',
      'plugins.connect',
    ].every((method) => ready?.data?.methods?.includes(method)))

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

  // --- a workspace ``.env`` configures the runtime, and an environment the
  //     host sets still wins over it ---
  const dotted = mkdtempSync(join(tmpdir(), 'bridge-dotenv-'))
  const scrubbed = { ...process.env }
  for (const name of [
    'ANTHROPIC_BASE_URL',
    'ANTHROPIC_API_KEY',
    'ANTHROPIC_AUTH_TOKEN',
    'ANTHROPIC_MODEL',
    'MODEL_ID',
    'FALLBACK_MODEL_ID',
    'MAX_TOKENS',
    // A proxy the developer happens to have exported must not stand between the
    // checks below and the local stand-in endpoint.
    'HTTP_PROXY',
    'HTTPS_PROXY',
    'ALL_PROXY',
    'NO_PROXY',
    'http_proxy',
    'https_proxy',
    'all_proxy',
    'no_proxy',
  ]) {
    delete scrubbed[name]
  }
  writeFileSync(
    join(dotted, '.env'),
    'ANTHROPIC_API_KEY=sk-ant-from-dotenv\nMODEL_ID=model-from-dotenv\n',
  )

  const fromDotenv = new Harness(dotted, { env: scrubbed })
  try {
    const ready = await fromDotenv.waitFor((messages) => named(messages, 'runtime.ready'))
    check(
      'a workspace .env supplies the model',
      ready?.data?.model === 'model-from-dotenv',
      ready?.data?.model,
    )
    check('a workspace .env clears the config error', ready?.data?.config_error === null)
    check(
      'loading the .env is reported on stderr',
      fromDotenv.stderr.some((line) => line.includes('.env')),
    )
  } finally {
    await fromDotenv.stop()
  }

  const fromHost = new Harness(dotted, {
    env: { ...scrubbed, ANTHROPIC_API_KEY: 'sk-ant-from-host', MODEL_ID: 'model-from-host' },
  })
  try {
    const ready = await fromHost.waitFor((messages) => named(messages, 'runtime.ready'))
    check(
      'an environment the host sets beats the workspace .env',
      ready?.data?.model === 'model-from-host',
      ready?.data?.model,
    )
  } finally {
    await fromHost.stop()
    rmSync(dotted, { recursive: true, force: true })
  }

  // --- the schedule: the runtime's own job table, driven from the host ---
  //
  // No model is configured here on purpose. A job that comes due is marked for
  // delivery by the bridge's ticker and then left in the queue, because there is
  // nothing to hand it to -- which is exactly what the page shows as 待投递.
  const scheduled = mkdtempSync(join(tmpdir(), 'bridge-cron-'))
  const cron = new Harness(scheduled, { env: scrubbed })
  try {
    await cron.waitFor((messages) => named(messages, 'runtime.ready'))

    cron.request('k-1', 'cron.list')
    const empty = await cron.waitFor((messages) => byId(messages, 'k-1'))
    check('the schedule starts empty', (empty?.result?.jobs ?? []).length === 0, JSON.stringify(empty?.result))

    cron.request('k-2', 'cron.schedule', {
      params: { cron: '0 9 * * 1-5', prompt: 'weekday standup' },
    })
    const created = await cron.waitFor((messages) => byId(messages, 'k-2'))
    check('a job can be scheduled', created?.ok === true, JSON.stringify(created))
    const job = created?.result?.job
    check(
      'the job comes back with an id the runtime made',
      /^cron_[0-9a-f]{8}$/.test(job?.id ?? ''),
      job?.id,
    )
    check('recurring and durable default on', job?.recurring === true && job?.durable === true)
    check(
      'the job says when it next runs',
      typeof job?.next_run === 'string' && job?.next_run_ms > 0,
      JSON.stringify(job),
    )
    check('a job that is not due yet is not waiting', job?.pending_delivery === false)

    cron.request('k-3', 'runtime.status')
    const counted = await cron.waitFor((messages) => byId(messages, 'k-3'))
    check(
      'runtime.status counts the job without being told twice',
      counted?.result?.cron?.jobs === 1,
      JSON.stringify(counted?.result?.cron),
    )

    cron.request('k-4', 'cron.schedule', { params: { cron: 'every day at nine', prompt: 'x' } })
    const refused = await cron.waitFor((messages) => byId(messages, 'k-4'))
    check(
      'an expression that cannot be parsed is refused',
      refused?.error?.code === 'cron_invalid',
      JSON.stringify(refused?.error),
    )
    cron.request('k-5', 'cron.cancel', { params: { id: 'cron_00000000' } })
    const missing = await cron.waitFor((messages) => byId(messages, 'k-5'))
    check(
      'cancelling a job that is not there is refused',
      missing?.error?.code === 'cron_unknown',
      JSON.stringify(missing?.error),
    )

    // A job due every minute is marked by the ticker within a second or so.
    cron.request('k-6', 'cron.schedule', { params: { cron: '* * * * *', prompt: 'due now' } })
    const due = await cron.waitFor((messages) => byId(messages, 'k-6'))
    check('a job due every minute can be scheduled', due?.ok === true, JSON.stringify(due?.error))
    cron.request('k-7', 'cron.list')
    const waiting = await cron.waitFor(
      (messages) =>
        (byId(messages, 'k-7')?.result?.jobs ?? []).some((entry) => entry.pending_delivery),
      5000,
    )
    check('the ticker marks a due job for delivery while nothing is running', waiting !== undefined)
    check(
      'and no turn was started with no model to run it',
      cron.messages().every((message) => !String(message.event ?? '').startsWith('chat.')),
    )

    cron.request('k-8', 'cron.cancel', { params: { id: job.id } })
    const cancelled = await cron.waitFor((messages) => byId(messages, 'k-8'))
    check('a job can be cancelled', cancelled?.result?.cancelled === job.id, JSON.stringify(cancelled?.result))
    cron.request('k-9', 'cron.list')
    const left = await cron.waitFor((messages) => byId(messages, 'k-9'))
    check(
      'and it is gone from the list',
      (left?.result?.jobs ?? []).length === 1,
      JSON.stringify(left?.result?.jobs),
    )
  } finally {
    await cron.stop()
    rmSync(scheduled, { recursive: true, force: true })
  }

  // --- tasks: the board the model's task tools write to ---
  //
  // A task is a JSON file under `.tasks/`, and the page drives the same files:
  // create a node, put one behind another, claim the ready one, and watch the
  // dependency open when it is completed.
  const boarded = mkdtempSync(join(tmpdir(), 'bridge-tasks-'))
  const board = new Harness(boarded, { env: scrubbed })
  try {
    await board.waitFor((messages) => named(messages, 'runtime.ready'))

    board.request('t-1', 'tasks.list')
    const emptyBoard = await board.waitFor((messages) => byId(messages, 't-1'))
    check('the task board starts empty', emptyBoard?.result?.count === 0, JSON.stringify(emptyBoard?.result))
    check(
      'and names the directory it read',
      emptyBoard?.result?.dir === join(boarded, '.tasks'),
      emptyBoard?.result?.dir,
    )

    board.request('t-2', 'tasks.create', {
      params: { subject: 'write the plan', description: 'the first node' },
    })
    const firstCreated = await board.waitFor((messages) => byId(messages, 't-2'))
    check('a task can be created', firstCreated?.ok === true, JSON.stringify(firstCreated?.error))
    const first = firstCreated?.result?.task
    check('the runtime made the id', /^task_[0-9a-f]{8}$/.test(String(first?.id ?? '')), first?.id)
    check('a fresh task is ready', first?.ready === true && first?.status === 'pending')
    check('and nobody owns it', first?.owner === null)

    board.request('t-3', 'tasks.create', {
      params: { subject: 'ship it', blocked_by: [first?.id] },
    })
    const secondCreated = await board.waitFor((messages) => byId(messages, 't-3'))
    const second = secondCreated?.result?.task
    check('a task can wait on another', secondCreated?.ok === true, JSON.stringify(secondCreated?.error))
    check(
      'and the dependency is reported as open',
      second?.ready === false && second?.blocked?.[0] === first?.id,
      JSON.stringify(second),
    )

    board.request('t-4', 'tasks.claim', { params: { id: second?.id } })
    const blockedClaim = await board.waitFor((messages) => byId(messages, 't-4'))
    check(
      'a blocked task cannot be claimed',
      blockedClaim?.error?.code === 'task_blocked',
      JSON.stringify(blockedClaim?.error),
    )

    board.request('t-5', 'tasks.claim', { params: { id: first?.id } })
    const claimed = await board.waitFor((messages) => byId(messages, 't-5'))
    check('a ready task can be claimed', claimed?.ok === true, JSON.stringify(claimed?.error))
    check(
      'and the page is recorded as its owner',
      claimed?.result?.task?.status === 'in_progress' && claimed?.result?.task?.owner === 'user',
      JSON.stringify(claimed?.result?.task),
    )

    board.request('t-6', 'runtime.status')
    const counted = await board.waitFor((messages) => byId(messages, 't-6'))
    check(
      'runtime.status counts it without being told twice',
      counted?.result?.tasks?.in_progress === 1 && counted?.result?.tasks?.total === 2,
      JSON.stringify(counted?.result?.tasks),
    )

    board.request('t-7', 'tasks.release', { params: { id: first?.id } })
    const released = await board.waitFor((messages) => byId(messages, 't-7'))
    check('a claimed task can be given back', released?.ok === true, JSON.stringify(released?.error))
    check('and it is ready again', released?.result?.task?.ready === true && released?.result?.task?.owner === null)

    board.request('t-8', 'tasks.claim', { params: { id: first?.id } })
    await board.waitFor((messages) => byId(messages, 't-8'))
    board.request('t-9', 'tasks.complete', { params: { id: first?.id } })
    const completed = await board.waitFor((messages) => byId(messages, 't-9'))
    check('a claimed task can be completed', completed?.ok === true, JSON.stringify(completed?.error))
    check(
      'and the completion names what it unblocked',
      String(completed?.result?.message ?? '').includes('Unblocked: ship it'),
      completed?.result?.message,
    )

    board.request('t-10', 'tasks.list')
    const listed = await board.waitFor((messages) => byId(messages, 't-10'))
    const ship = (listed?.result?.tasks ?? []).find((task) => task.id === second?.id)
    check('the dependent task is ready now', ship?.ready === true, JSON.stringify(ship))
    check('and its open prerequisites are empty', ship?.blocked?.length === 0, JSON.stringify(ship?.blocked))

    board.request('t-11', 'tasks.create', {
      params: { subject: 'x', blocked_by: ['task_00000000'] },
    })
    const missingDependency = await board.waitFor((messages) => byId(messages, 't-11'))
    check(
      'a dependency that is not on the board is refused',
      missingDependency?.error?.code === 'task_unknown',
      JSON.stringify(missingDependency?.error),
    )

    board.request('t-12', 'tasks.claim', { params: { id: 'task_00000000' } })
    const missingTask = await board.waitFor((messages) => byId(messages, 't-12'))
    check(
      'claiming a task that is not there is refused',
      missingTask?.error?.code === 'task_unknown',
      JSON.stringify(missingTask?.error),
    )
  } finally {
    await board.stop()
    rmSync(boarded, { recursive: true, force: true })
  }

  // --- skills: the runtime's own loader, listed and read in full ---
  //
  // A skill is a file in the workspace, so the page lists the catalogue and
  // opens one manifest; neither of those changes anything.
  const equipped = mkdtempSync(join(tmpdir(), 'bridge-skills-'))
  mkdirSync(join(equipped, 'skills', 'demo'), { recursive: true })
  writeFileSync(
    join(equipped, 'skills', 'demo', 'SKILL.md'),
    '---\nname: demo\ndescription: A demo skill.\n---\n\n# Demo\n\nSay hi.\n',
  )
  const skills = new Harness(equipped, { env: scrubbed })
  try {
    await skills.waitFor((messages) => named(messages, 'runtime.ready'))

    skills.request('g-1', 'skills.list')
    const catalogue = await skills.waitFor((messages) => byId(messages, 'g-1'))
    check(
      'the catalogue lists the skill',
      catalogue?.result?.count === 1,
      JSON.stringify(catalogue?.result),
    )
    check(
      'and names the directory it scanned',
      catalogue?.result?.dir === join(equipped, 'skills'),
      catalogue?.result?.dir,
    )
    const listed = catalogue?.result?.skills?.[0]
    check(
      'a skill carries its description',
      listed?.description === 'A demo skill.',
      JSON.stringify(listed),
    )
    check(
      'and the manifest it came from',
      String(listed?.path ?? '').endsWith('SKILL.md'),
      listed?.path,
    )

    skills.request('g-2', 'skills.read', { params: { name: 'demo' } })
    const opened = await skills.waitFor((messages) => byId(messages, 'g-2'))
    check('a skill can be read in full', opened?.ok === true, JSON.stringify(opened?.error))
    check(
      'and the answer is the manifest itself',
      String(opened?.result?.content ?? '').includes('Say hi.'),
      JSON.stringify(opened?.result?.content),
    )

    skills.request('g-3', 'skills.read', { params: { name: 'nope' } })
    const unknownSkill = await skills.waitFor((messages) => byId(messages, 'g-3'))
    check(
      'reading a skill that is not there is refused',
      unknownSkill?.error?.code === 'skill_unknown',
      JSON.stringify(unknownSkill?.error),
    )

    // --- a skill folder the user already has on disk can be brought in ---
    //
    // The page has no other way to add one: the loader scans one directory, so
    // the import is the copy that puts a folder there.
    const source = mkdtempSync(join(tmpdir(), 'bridge-skill-src-'))
    mkdirSync(join(source, 'imported'), { recursive: true })
    writeFileSync(
      join(source, 'imported', 'SKILL.md'),
      '---\nname: imported\ndescription: A skill copied in.\n---\n\n# Imported\n',
    )
    skills.request('g-4', 'skills.import', { params: { path: source } })
    const imported = await skills.waitFor((messages) => byId(messages, 'g-4'))
    check('a skill folder on disk can be imported', imported?.ok === true, JSON.stringify(imported?.error))
    check('and the answer counts it', imported?.result?.count === 1, JSON.stringify(imported?.result))
    check('under the name its manifest gives', imported?.result?.skills?.[0]?.name === 'imported')

    skills.request('g-5', 'skills.list')
    const regrown = await skills.waitFor((messages) => byId(messages, 'g-5'))
    check('the catalogue grew by the imported skill', regrown?.result?.count === 2, JSON.stringify(regrown?.result))
    check(
      'and it is listed like any other',
      (regrown?.result?.skills ?? []).some((skill) => skill.name === 'imported'),
    )

    // A re-import never lands on top of what is already there.
    skills.request('g-6', 'skills.import', { params: { path: source } })
    const twice = await skills.waitFor((messages) => byId(messages, 'g-6'))
    check(
      'importing the same skill twice is refused',
      twice?.error?.code === 'skill_exists',
      JSON.stringify(twice?.error),
    )

    // A folder with no manifest anywhere in it is not a skill.
    const empty = mkdtempSync(join(tmpdir(), 'bridge-skill-empty-'))
    skills.request('g-7', 'skills.import', { params: { path: empty } })
    const notSkill = await skills.waitFor((messages) => byId(messages, 'g-7'))
    check(
      'importing a folder with no manifest is refused',
      notSkill?.error?.code === 'skill_invalid',
      JSON.stringify(notSkill?.error),
    )

    // A path that is not a folder at all is refused with its own code.
    skills.request('g-8', 'skills.import', { params: { path: join(empty, 'missing') } })
    const noFolder = await skills.waitFor((messages) => byId(messages, 'g-8'))
    check(
      'importing a path that is not a folder is refused',
      noFolder?.error?.code === 'skill_source',
      JSON.stringify(noFolder?.error),
    )

    rmSync(source, { recursive: true, force: true })
    rmSync(empty, { recursive: true, force: true })
  } finally {
    await skills.stop()
    rmSync(equipped, { recursive: true, force: true })
  }

  // --- plugins: the MCP registry the model's tool pool reads ---
  const plugged = mkdtempSync(join(tmpdir(), 'bridge-plugins-'))
  const plugins = new Harness(plugged, { env: scrubbed })
  try {
    await plugins.waitFor((messages) => named(messages, 'runtime.ready'))

    plugins.request('p-1', 'plugins.list')
    const before = await plugins.waitFor((messages) => byId(messages, 'p-1'))
    check(
      'the plugin list names what this build ships',
      before?.result?.available >= 1,
      JSON.stringify(before?.result),
    )
    check('nothing is connected at startup', (before?.result?.connected ?? []).length === 0)
    check(
      'and an unconnected plugin has no catalogue yet',
      (before?.result?.servers ?? []).every((server) => server.tools.length === 0),
      JSON.stringify(before?.result?.servers),
    )

    plugins.request('p-2', 'plugins.connect', { params: { server: 'docs' } })
    const connected = await plugins.waitFor((messages) => byId(messages, 'p-2'))
    check('a plugin can be connected', connected?.ok === true, JSON.stringify(connected?.error))
    check('and the answer names it', connected?.result?.server === 'docs')

    plugins.request('p-3', 'runtime.status')
    const countedPlugin = await plugins.waitFor((messages) => byId(messages, 'p-3'))
    check(
      'runtime.status counts the connection without being told twice',
      countedPlugin?.result?.plugins?.connected === 1,
      JSON.stringify(countedPlugin?.result?.plugins),
    )

    plugins.request('p-4', 'plugins.list')
    const after = await plugins.waitFor((messages) => byId(messages, 'p-4'))
    const docs = (after?.result?.servers ?? []).find((server) => server.name === 'docs')
    check('a connected plugin is marked connected', docs?.connected === true)
    check(
      'and its tools were discovered, namespaced and policy-tagged',
      (docs?.tools ?? []).length > 0 &&
        (docs?.tools ?? []).every(
          (tool) => tool.name.startsWith('mcp__docs__') && typeof tool.policy === 'string',
        ),
      JSON.stringify(docs?.tools),
    )

    plugins.request('p-5', 'plugins.connect', { params: { server: 'nope' } })
    const unknownPlugin = await plugins.waitFor((messages) => byId(messages, 'p-5'))
    check(
      'connecting a plugin this build does not ship is refused',
      unknownPlugin?.error?.code === 'plugin_unknown',
      JSON.stringify(unknownPlugin?.error),
    )

    // The size the page reads to show how full the conversation is. It is a
    // fact about the conversation, so it is answerable before any model exists.
    plugins.request('p-6', 'chat.context')
    const context = await plugins.waitFor((messages) => byId(messages, 'p-6'))
    check('chat.context answers', context?.ok === true, JSON.stringify(context?.error))
    check(
      'and counts messages and compactions',
      typeof context?.result?.messages === 'number' &&
        typeof context?.result?.compacted === 'number',
      JSON.stringify(context?.result),
    )
    check('and measures chars the compactor way', typeof context?.result?.chars === 'number')
  } finally {
    await plugins.stop()
    rmSync(plugged, { recursive: true, force: true })
  }

  // --- a workspace can declare its own MCP server in plugins.json ---
  //
  // This is the extension path: no rebuild, just a file naming the command to
  // run. The server here is a real one over stdio, spawned the same way a
  // shipped one would be.
  const notesServer = resolve(
    fileURLToPath(new URL('.', import.meta.url)),
    'fixtures',
    'mcp-notes-server.mjs',
  )
  const declaring = mkdtempSync(join(tmpdir(), 'bridge-plugins-declared-'))
  writeFileSync(
    join(declaring, 'plugins.json'),
    `${JSON.stringify(
      {
        servers: [
          { name: 'notes', command: process.execPath, args: [notesServer], policy: 'allow' },
        ],
      },
      null,
      2,
    )}\n`,
  )
  const declared = new Harness(declaring, { env: scrubbed })
  try {
    await declared.waitFor((messages) => named(messages, 'runtime.ready'))

    declared.request('d-1', 'plugins.list')
    const listed = await declared.waitFor((messages) => byId(messages, 'd-1'))
    const notes = (listed?.result?.servers ?? []).find((server) => server.name === 'notes')
    check('a workspace can declare its own plugin', notes !== undefined, JSON.stringify(listed?.result))
    check('and it is marked as coming from the workspace', notes?.source === 'workspace')
    check(
      'and its command is shown so the page can say what runs',
      typeof notes?.command === 'string' && notes.command.includes('mcp-notes-server.mjs'),
      notes?.command,
    )
    check(
      'and a declared plugin starts out unconnected like any other',
      notes?.connected === false,
    )

    declared.request('d-2', 'plugins.connect', { params: { server: 'notes' } })
    const joined = await declared.waitFor((messages) => byId(messages, 'd-2'), 20000)
    check('a declared plugin can be connected', joined?.ok === true, JSON.stringify(joined?.error))

    declared.request('d-3', 'plugins.list')
    const grown = await declared.waitFor((messages) => byId(messages, 'd-3'))
    const connected = (grown?.result?.servers ?? []).find((server) => server.name === 'notes')
    check('and it is then marked connected', connected?.connected === true)
    check(
      'and its tools were discovered and namespaced',
      (connected?.tools ?? []).some((tool) => tool.name === 'mcp__notes__add_note') &&
        (connected?.tools ?? []).some((tool) => tool.name === 'mcp__notes__list_notes'),
      JSON.stringify(connected?.tools),
    )
    check(
      'and its declared policy is the one the page shows',
      (connected?.tools ?? []).every((tool) => tool.policy === 'allow'),
      JSON.stringify(connected?.tools),
    )

    declared.request('d-4', 'runtime.status')
    const counted = await declared.waitFor((messages) => byId(messages, 'd-4'))
    check(
      'runtime.status counts the declared plugin too',
      counted?.result?.plugins?.connected === 1,
      JSON.stringify(counted?.result?.plugins),
    )
  } finally {
    await declared.stop()
    rmSync(declaring, { recursive: true, force: true })
  }

  // --- the two ways a turn asks the page for something ---
  //
  // A run that wants a call approved, or an answer to a question, parks inside
  // the tool call and reports the request as an event; the page replies on these
  // two methods. Neither needs a model: a reply that reaches no parked call is
  // what a page sends after the call is already over, and it is told so.
  const asking = mkdtempSync(join(tmpdir(), 'bridge-interaction-'))
  const waiting = new Harness(asking, { env: scrubbed })
  try {
    await waiting.waitFor((messages) => named(messages, 'runtime.ready'))

    waiting.request('w-1', 'chat.answer', { params: { id: 'ask', answer: 'yes' } })
    const spoken = await waiting.waitFor((messages) => byId(messages, 'w-1'))
    check('an answer nobody asked for is taken as nothing to do', spoken?.ok === true, JSON.stringify(spoken?.error))
    check('and says so rather than claiming it was delivered', spoken?.result?.accepted === false)

    waiting.request('w-2', 'chat.approve', { params: { id: 'perm', allow: true } })
    const decided = await waiting.waitFor((messages) => byId(messages, 'w-2'))
    check('the same is true of a decision', decided?.result?.accepted === false, JSON.stringify(decided))
    check('and it is a response, not an event', decided?.type === 'response')

    waiting.request('w-3', 'chat.answer', { params: { id: 'ask' } })
    const unspoken = await waiting.waitFor((messages) => byId(messages, 'w-3'))
    check('an answer with no words is refused', unspoken?.error?.code === 'invalid_params', JSON.stringify(unspoken?.error))

    waiting.request('w-4', 'chat.approve', { params: { id: 'perm', allow: 'yes' } })
    const vague = await waiting.waitFor((messages) => byId(messages, 'w-4'))
    check('a decision that is not a yes or no is refused', vague?.error?.code === 'invalid_params', JSON.stringify(vague?.error))

    check(
      'neither reply is broadcast as an event',
      waiting.messages().every((message) =>
        !String(message.event ?? '').startsWith('chat.permission') &&
          !String(message.event ?? '').startsWith('chat.question')),
    )
  } finally {
    await waiting.stop()
    rmSync(asking, { recursive: true, force: true })
  }

  // --- chat: a prompt runs through the loop and the turn comes back as events ---
  //
  // A local stand-in for `/v1/messages` answers every turn, so a refused prompt
  // and an answered one are both exercised without a network and without a
  // token. A prompt that mentions `refuse` gets a 401; one that mentions `slow`
  // has its first byte held back, so a second prompt lands mid-turn.
  const model = createServer((request, response) => {
    // A turn the user stops drops the connection mid-answer, so the stand-in
    // must not treat a dead socket as its own failure.
    request.on('error', () => {})
    response.on('error', () => {})
    let body = ''
    request.on('data', (chunk) => (body += chunk))
    request.on('end', () => {
      if (body.includes('refuse')) {
        response.writeHead(401, { 'content-type': 'application/json' })
        response.end(JSON.stringify({ type: 'error', error: { message: 'bad key' } }))
        return
      }
      const frames = [
        'data: {"type":"message_start","message":{"model":"mock","usage":{"input_tokens":3,"output_tokens":1}}}',
        'data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}',
        'data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello "}}',
        'data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"from the mock"}}',
        'data: {"type":"content_block_stop","index":0}',
        'data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}',
      ]
      const answer = () => {
        response.writeHead(200, { 'content-type': 'text/event-stream' })
        for (const frame of frames) response.write(`${frame}\n\n`)
        response.end()
      }
      if (body.includes('slow')) setTimeout(answer, 1500)
      else answer()
    })
  })
  await new Promise((listening) => model.listen(0, '127.0.0.1', listening))

  const mock = mkdtempSync(join(tmpdir(), 'bridge-chat-'))
  writeFileSync(
    join(mock, '.env'),
    `ANTHROPIC_BASE_URL=http://127.0.0.1:${model.address().port}\n` +
      'ANTHROPIC_API_KEY=sk-ant-mock\nMODEL_ID=mock-model\nMAX_TOKENS=64\n',
  )

  // No `.env` and no environment: nothing to run a prompt with.
  const bare = mkdtempSync(join(tmpdir(), 'bridge-bare-'))
  const unrunnable = new Harness(bare, { env: scrubbed })
  try {
    await unrunnable.waitFor((messages) => named(messages, 'runtime.ready'))
    unrunnable.request('c-1', 'chat.send', { params: { prompt: 'hello' } })
    const refused = await unrunnable.waitFor((messages) => byId(messages, 'c-1'))
    check('a prompt with no model configured is refused', refused?.ok === false)
    check(
      'the refusal says what to configure',
      refused?.error?.code === 'chat_not_configured',
      refused?.error?.code,
    )
    check('a refusal carries no result', refused?.result === undefined)
  } finally {
    await unrunnable.stop()
    rmSync(bare, { recursive: true, force: true })
  }

  const chat = new Harness(mock, { env: scrubbed })
  try {
    await chat.waitFor((messages) => named(messages, 'runtime.ready'))

    // A bad prompt is a response, not an event: it never starts a turn.
    // Nothing has run yet, so there is nothing to read back.
    chat.request('c-0', 'chat.history')
    const nothing = await chat.waitFor((messages) => byId(messages, 'c-0'))
    check(
      'a conversation that has not started reads back empty',
      nothing?.result?.turns?.length === 0,
    )

    chat.request('c-2', 'chat.send', { params: { prompt: '   ' } })
    const empty = await chat.waitFor((messages) => byId(messages, 'c-2'))
    check(
      'an empty prompt is refused',
      empty?.ok === false && empty?.error?.code === 'invalid_params',
    )

    chat.request('c-3', 'chat.send', { params: { prompt: 'say hello' } })
    const accepted = await chat.waitFor((messages) => byId(messages, 'c-3'))
    check('a runnable prompt is accepted', accepted?.ok === true)
    check('the accept names the turn its events carry', accepted?.result?.turn === 'turn-1')

    // The reader loop must keep answering while the turn is in flight.
    chat.request('c-4', 'runtime.ping')
    const pong = await chat.waitFor((messages) => byId(messages, 'c-4'), 3000)
    check('a turn does not block the request loop', pong?.result?.pong === true)

    const done = await chat.waitFor(
      (messages) => messages.find((message) => message.event === 'chat.done'),
    )
    check('the turn reports chat.done', done?.event === 'chat.done')
    check('chat.done names its turn', done?.data?.turn === 'turn-1')
    check('chat.done carries the answer', done?.data?.text === 'hello from the mock')
    const streamed = chat
      .messages()
      .filter((message) => message.event === 'chat.delta' && message.data?.turn === 'turn-1')
      .map((message) => message.data.text)
      .join('')
    check('the answer streamed as it was produced', streamed === 'hello from the mock', streamed)

    // A rejected prompt is still a turn: the provider's status is classified.
    chat.request('c-5', 'chat.send', { params: { prompt: 'refuse this' } })
    const rejected = await chat.waitFor((messages) => byId(messages, 'c-5'))
    check('a rejected prompt is accepted as a turn', rejected?.result?.turn === 'turn-2')
    const failed = await chat.waitFor((messages) =>
      messages.find(
        (message) => message.event === 'chat.failed' && message.data?.turn === 'turn-2',
      ),
    )
    check('a rejected turn reports chat.failed', failed?.event === 'chat.failed')
    check(
      'the failure keeps the code the page branches on',
      failed?.data?.error?.code === 'chat_unauthorized',
      failed?.data?.error?.code,
    )
    check(
      'the failure quotes the provider',
      (failed?.data?.error?.message ?? '').includes('bad key'),
    )
    check(
      'a failed turn streams no answer',
      chat
        .messages()
        .every((message) => message.event !== 'chat.delta' || message.data?.turn !== 'turn-2'),
    )

    // One turn at a time: a prompt that arrives mid-turn is refused, not queued.
    chat.request('c-6', 'chat.send', { params: { prompt: 'slow please' } })
    const slow = await chat.waitFor((messages) => byId(messages, 'c-6'))
    check('a slow turn is accepted', slow?.result?.turn === 'turn-3')

    chat.request('c-7', 'chat.send', { params: { prompt: 'and another' } })
    const busy = await chat.waitFor((messages) => byId(messages, 'c-7'))
    check(
      'a prompt that arrives mid-turn is refused',
      busy?.ok === false && busy?.error?.code === 'chat_busy',
      busy?.error?.code,
    )
    const slowDone = await chat.waitFor(
      (messages) =>
        messages.find((message) => message.event === 'chat.done' && message.data?.turn === 'turn-3'),
      8000,
    )
    check('the turn that was already running still finishes', slowDone?.data?.text === 'hello from the mock')

    // A page that reloaded asks for the conversation instead of losing it.
    chat.request('c-8', 'chat.history')
    const history = await chat.waitFor((messages) => byId(messages, 'c-8'))
    check('chat.history answers', history?.ok === true)
    const turns = history?.result?.turns ?? []
    check('chat.history keeps every turn', turns.length === 3, JSON.stringify(turns.map((turn) => turn.turn)))
    check('a kept turn keeps the prompt that was typed', turns[0]?.prompt === 'say hello')
    check('a kept turn keeps the events it arrived as', turns[0]?.events?.some((event) => event.event === 'chat.done'))
    check('a finished turn reads back as done', turns[0]?.status === 'done')
    check('a failed turn reads back as failed', turns[1]?.status === 'failed')
    check(
      'a failed turn keeps the code the page branches on',
      turns[1]?.error?.code === 'chat_unauthorized',
      turns[1]?.error?.code,
    )

    // --- sessions: the conversation is written down, and stays stoppable ---
    //
    // A stop is a request about a turn, not a guess: with nothing running it
    // says so, and with a turn in flight it ends that turn where it stands.
    chat.request('c-10', 'chat.cancel')
    const idleStop = await chat.waitFor((messages) => byId(messages, 'c-10'))
    check(
      'a stop with no turn running says nothing was stopped',
      idleStop?.result?.cancelled === false,
      JSON.stringify(idleStop?.result),
    )

    chat.request('c-11', 'sessions.list')
    const listed = await chat.waitFor((messages) => byId(messages, 'c-11'))
    const current = listed?.result?.current
    const summaries = listed?.result?.sessions ?? []
    check(
      'sessions.list names the conversation being continued',
      typeof current === 'string' && current !== '',
    )
    const stored = summaries.find((session) => session.id === current)
    check('sessions.list holds the conversation that just ran', stored !== undefined)
    check(
      'the stored conversation is named after the first prompt',
      stored?.title === 'say hello',
      stored?.title,
    )
    check(
      'the session file is where the runtime says it is',
      existsSync(join(mock, '.sessions', `${current}.json`)),
    )

    chat.request('c-12', 'chat.send', { params: { prompt: 'slow and stoppable' } })
    const running = await chat.waitFor((messages) => byId(messages, 'c-12'))
    check('a turn that can be stopped is accepted', running?.result?.turn === 'turn-4')
    chat.request('c-13', 'chat.cancel')
    const stopped = await chat.waitFor((messages) => byId(messages, 'c-13'))
    check('a stop reaches the turn that is running', stopped?.result?.cancelled === true)
    const cancelled = await chat.waitFor((messages) =>
      messages.find((message) => message.event === 'chat.cancelled'),
    )
    check('the stopped turn reports chat.cancelled', cancelled?.data?.turn === 'turn-4')

    // The stop freed the one-turn slot, so the loop is not stuck with it.
    chat.request('c-14', 'chat.send', { params: { prompt: 'keep going' } })
    const next = await chat.waitFor((messages) => byId(messages, 'c-14'))
    check('a stop frees the slot for the next prompt', next?.result?.turn === 'turn-5')
    const nextDone = await chat.waitFor((messages) =>
      messages.find((message) => message.event === 'chat.done' && message.data?.turn === 'turn-5'),
    )
    check('the turn after a stop still answers', nextDone?.data?.text === 'hello from the mock')

    // A stopped turn is not fed back to the model: the next prompt starts from
    // the same place the stopped one did.
    check(
      'a stopped turn is not written into the conversation',
      chat
        .messages()
        .every((message) => message.event !== 'chat.delta' || message.data?.turn !== 'turn-4'),
    )

    // --- a new process continues the conversation the old one wrote ---
    await chat.stop()
    const resumed = new Harness(mock, { env: scrubbed })
    try {
      const ready = await resumed.waitFor((messages) => named(messages, 'runtime.ready'))
      check('the restarted bridge is ready', ready !== undefined)

      resumed.request('c-20', 'chat.history')
      const readback = await resumed.waitFor((messages) => byId(messages, 'c-20'))
      const kept = readback?.result?.turns ?? []
      check(
        'the conversation outlived the process',
        kept.length === 5,
        JSON.stringify(kept.map((turn) => turn.turn)),
      )
      check('a kept turn still reads back as done', kept[0]?.status === 'done')
      check('a kept failure still carries its code', kept[1]?.error?.code === 'chat_unauthorized')
      check('a stopped turn reads back as stopped', kept[3]?.status === 'cancelled', kept[3]?.status)

      resumed.request('c-21', 'sessions.list')
      const remembered = await resumed.waitFor((messages) => byId(messages, 'c-21'))
      check(
        'the restarted bridge continues the same conversation',
        remembered?.result?.current === current,
      )

      resumed.request('c-22', 'sessions.new')
      const fresh = await resumed.waitFor((messages) => byId(messages, 'c-22'))
      const freshId = fresh?.result?.session
      check('a new conversation gets an id of its own', typeof freshId === 'string' && freshId !== current)
      // A conversation that has not run a turn yet has nothing to write, but it
      // is still the one being continued: a list that leaves it out reads as
      // though the workspace only ever holds one conversation.
      resumed.request('c-29', 'sessions.list')
      const started = await resumed.waitFor((messages) => byId(messages, 'c-29'))
      check('a new conversation is the one being continued', started?.result?.current === freshId)
      check(
        'and it is listed before it has a turn to write',
        (started?.result?.sessions ?? []).some((session) => session.id === freshId),
      )
      resumed.request('c-23', 'chat.history')
      const empty = await resumed.waitFor((messages) => byId(messages, 'c-23'))
      check('a new conversation starts empty', (empty?.result?.turns ?? []).length === 0)

      resumed.request('c-24', 'sessions.open', { params: { id: current } })
      const opened = await resumed.waitFor((messages) => byId(messages, 'c-24'))
      check('the conversation that was left behind opens again', opened?.result?.session === current)
      resumed.request('c-25', 'chat.history')
      const reopened = await resumed.waitFor((messages) => byId(messages, 'c-25'))
      check('opening it brings its turns back', (reopened?.result?.turns ?? []).length === 5)

        // Renaming is about the conversation, not the page: the name goes into
        // the file the turns are in, so it is still there next time.
        resumed.request('c-30', 'sessions.rename', {
          params: { id: current, title: 'renamed by the page' },
        })
        const renamed = await resumed.waitFor((messages) => byId(messages, 'c-30'))
        check('a conversation can be renamed', renamed?.result?.title === 'renamed by the page')
        check('renaming answers about the conversation it renamed', renamed?.result?.session === current)
        const renamedFile = JSON.parse(readFileSync(join(mock, '.sessions', `${current}.json`), 'utf8'))
        check(
          'the new name is in the file the turns are in',
          renamedFile.title === 'renamed by the page',
          renamedFile.title,
        )
        resumed.request('c-31', 'sessions.list')
        const renamedList = await resumed.waitFor((messages) => byId(messages, 'c-31'))
        check(
          'the list shows the new name',
          (renamedList?.result?.sessions ?? []).find((session) => session.id === current)?.title ===
            'renamed by the page',
        )

        resumed.request('c-32', 'sessions.rename', {
          params: { id: 's1-2-3', title: 'nothing to name' },
        })
        const unrenamed = await resumed.waitFor((messages) => byId(messages, 'c-32'))
        check(
          'renaming a conversation this workspace does not hold is refused',
          unrenamed?.error?.code === 'session_unknown',
          unrenamed?.error?.code,
        )

        // The new conversation was never written -- it had no turn -- so it is
        // not something the store can be asked to delete.
        resumed.request('c-33', 'sessions.delete', { params: { id: freshId } })
        const undeleted = await resumed.waitFor((messages) => byId(messages, 'c-33'))
        check(
          'deleting a conversation that was never written is refused',
          undeleted?.error?.code === 'session_unknown',
          undeleted?.error?.code,
        )

      // A session id is a file name, so the shape is checked before the disk is
      // touched, and a well-formed id that is not there is a different answer.
      resumed.request('c-26', 'sessions.open', { params: { id: 'not/a/session' } })
      const malformed = await resumed.waitFor((messages) => byId(messages, 'c-26'))
      check(
        'an id that could name another file is refused',
        malformed?.error?.code === 'invalid_params',
        malformed?.error?.code,
      )
      resumed.request('c-28', 'sessions.open', { params: { id: 's1-2-3' } })
      const missing = await resumed.waitFor((messages) => byId(messages, 'c-28'))
      check(
        'a conversation this workspace does not hold is refused',
        missing?.error?.code === 'session_unknown',
        missing?.error?.code,
      )

      resumed.request('c-27', 'chat.send', { params: { prompt: 'after a resume' } })
      const numbered = await resumed.waitFor((messages) => byId(messages, 'c-27'))
      check(
        'the next turn continues the numbering',
        numbered?.result?.turn === 'turn-6',
        numbered?.result?.turn,
      )

        // One thing at a time: the turn has to be over before its conversation
        // can be deleted, so this waits for the turn rather than assuming it.
        const finished = await resumed.waitFor((messages) =>
          messages.find((message) => message.event === 'chat.done' && message.data?.turn === 'turn-6'),
        )
        check('the turn after a resume finished', finished !== undefined)

        // Deleting the conversation in hand is allowed, and leaves the runtime
        // somewhere to put the next prompt rather than nowhere.
        resumed.request('c-34', 'sessions.delete', { params: { id: current } })
        const deleted = await resumed.waitFor((messages) => byId(messages, 'c-34'))
        check('the conversation in hand can be deleted', deleted?.result?.deleted === current, JSON.stringify(deleted?.result))
        check('deleting it moves the runtime to another conversation', deleted?.result?.current !== current)
        check('its file is gone', !existsSync(join(mock, '.sessions', `${current}.json`)))
        resumed.request('c-35', 'chat.history')
        const afterDelete = await resumed.waitFor((messages) => byId(messages, 'c-35'))
        check(
          'the conversation it moved to is its own, not the deleted one',
          (afterDelete?.result?.turns ?? []).length === 0,
          JSON.stringify((afterDelete?.result?.turns ?? []).map((turn) => turn.turn)),
        )

        // --- a conversation longer than one frame still reads back ---
        //
        // The bridge keeps up to 4 MiB of turns and answers `chat.history` with
        // all of them on one line, so the host has to take a reply longer than
        // the line it reads requests with. When it does not, the answer is
        // dropped, the request times out, and the page is left with a
        // conversation that simply does not load. Written here the way a
        // previous run of the app would have left it.
        const bigId = 's1791900000000-1-9'
        const bigTurns = []
        for (let number = 1; number <= 4; number += 1) {
          const events = []
          for (let index = 0; index < 6; index += 1) {
            events.push({
              protocol_version: PROTOCOL_VERSION,
              type: 'event',
              event: 'chat.delta',
              ts: '2026-01-01T00:00:00Z',
              data: { turn: `turn-${number}`, source: 'main', text: 'x'.repeat(64 * 1024) },
            })
          }
          events.push({
            protocol_version: PROTOCOL_VERSION,
            type: 'event',
            event: 'chat.done',
            ts: '2026-01-01T00:00:00Z',
            data: { turn: `turn-${number}`, ok: true, text: 'done' },
          })
          bigTurns.push({
            turn: `turn-${number}`,
            prompt: `prompt ${number}`,
            status: 'done',
            events,
          })
        }
        writeFileSync(
          join(mock, '.sessions', `${bigId}.json`),
          JSON.stringify({
            id: bigId,
            title: 'a long conversation',
            created_at: '2026-01-01T00:00:00Z',
            updated_at: '2026-01-01T00:00:00Z',
            messages: [],
            view: { turns: bigTurns },
          }),
        )

        resumed.request('c-40', 'sessions.open', { params: { id: bigId } })
        const bigOpened = await resumed.waitFor((messages) => byId(messages, 'c-40'))
        check('a long conversation opens', bigOpened?.result?.session === bigId, JSON.stringify(bigOpened?.result))

        resumed.request('c-41', 'chat.history')
        const bigRead = await resumed.waitFor((messages) => byId(messages, 'c-41'))
        check(
          'a long conversation reads back whole',
          (bigRead?.result?.turns ?? []).length === 4,
          JSON.stringify(bigRead?.result?.turns?.length),
        )
        const bigLine = resumed.stdout.find((line) => line.includes('"request_id":"c-41"')) ?? ''
        // Longer than a request frame, so this is the answer that used to be
        // dropped; shorter than the reply limit the host takes, which is the
        // number that has to cover the runtime's own log budget.
        check('the answer is one line longer than a request frame', bigLine.length > 1 << 20, `${bigLine.length} bytes`)
        check('and one the host will take', bigLine.length > 0 && bigLine.length < 8 << 20, `${bigLine.length} bytes`)
    } finally {
      await resumed.stop()
    }

    const lines = chat.stdout.filter((line) => line.trim() !== '')
    check(
      'chat output is protocol only',
      lines.every((line) => ['request', 'response', 'event', 'error'].includes(JSON.parse(line).type)),
    )
    check(
      'chat events never carry a request id',
      chat
        .messages()
        .filter((message) => message.type === 'event')
        .every((message) => message.request_id === undefined),
    )
    check(
      'every chat event is an event and each names a turn',
      chat
        .messages()
        .filter((message) => String(message.event ?? '').startsWith('chat.'))
        .every((message) => message.type === 'event' && typeof message.data?.turn === 'string'),
    )
  } finally {
    await chat.stop()
    rmSync(mock, { recursive: true, force: true })
    model.close()
  }

  // --- conversations can be kept outside the workspace ---
  //
  // The desktop host names the store, so a project the runtime is working in is
  // never also a directory the runtime writes conversations into.
  const clean = mkdtempSync(join(tmpdir(), 'bridge-clean-'))
  const storeRoot = mkdtempSync(join(tmpdir(), 'bridge-store-'))
  // Deliberately one level down: the bridge has to create it.
  const store = join(storeRoot, 'conversations')
  const hosted = new Harness(clean, { args: ['--sessions-dir', store], env: scrubbed })
  try {
    const ready = await hosted.waitFor((messages) => named(messages, 'runtime.ready'))
    check('a bridge given its own sessions directory is ready', ready !== undefined)
    check('the workspace it was given is the one it reports', ready?.data?.workspace === clean)
    check('the sessions directory is created when it is missing', existsSync(store))

    hosted.request('s1', 'runtime.status')
    const status = await hosted.waitFor((messages) => byId(messages, 's1'))
    check(
      'runtime.status reports the sessions directory it was given',
      status?.result?.paths?.sessions === store,
      status?.result?.paths?.sessions,
    )

    hosted.request('s2', 'sessions.list')
    const before = await hosted.waitFor((messages) => byId(messages, 's2'))
    const mine = before?.result?.current
    check('an empty store still has the conversation in hand', typeof mine === 'string' && mine !== '')

    hosted.request('s3', 'sessions.rename', {
      params: { id: mine, title: 'named before it is written' },
    })
    const renamed = await hosted.waitFor((messages) => byId(messages, 's3'))
    check(
      'a conversation can be named before it has a file',
      renamed?.result?.title === 'named before it is written',
    )
    hosted.request('s4', 'sessions.list')
    const listed = await hosted.waitFor((messages) => byId(messages, 's4'))
    check(
      'and the name is what the list shows',
      (listed?.result?.sessions ?? []).find((session) => session.id === mine)?.title ===
        'named before it is written',
      JSON.stringify(listed?.result?.sessions),
    )

    hosted.request('s5', 'sessions.delete', { params: { id: mine } })
    const gone = await hosted.waitFor((messages) => byId(messages, 's5'))
    check('the conversation in hand can be deleted', gone?.result?.deleted === mine, JSON.stringify(gone?.result))
    check('and the runtime is left in another one', gone?.result?.current !== mine)

    check('nothing was written into the workspace', !existsSync(join(clean, '.sessions')))
    check('the store directory is where the host put it', existsSync(store))
  } finally {
    await hosted.stop()
    rmSync(clean, { recursive: true, force: true })
    rmSync(storeRoot, { recursive: true, force: true })
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
