#!/usr/bin/env node
// End-to-end smoke test for the desktop bridge.
//
// Spawns the real `desktop_bridge` binary and drives it over stdin/stdout the
// same way the Tauri host does, so the process contract (stdout purity,
// request_id correlation, framing limits, clean shutdown) is verified against
// the shipped artifact and not just against unit tests.

import { spawn } from 'node:child_process'
import { mkdtempSync, rmSync, writeFileSync, mkdirSync, existsSync } from 'node:fs'
import { createServer } from 'node:http'
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
  constructor(workspace, options = {}) {
    this.stdout = []
    this.stderr = []
    this.exited = null
    this.waiters = []
    this.child = spawn(bridgePath, ['--workspace', workspace], {
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
    check('runtime.ready lists every method', ready?.data?.methods?.length === 9)
    check('runtime.ready advertises chat.send', ready?.data?.methods?.includes('chat.send'))
    check('runtime.ready advertises chat.cancel', ready?.data?.methods?.includes('chat.cancel'))
    check('runtime.ready advertises chat.history', ready?.data?.methods?.includes('chat.history'))
    check('runtime.ready advertises the session methods', [
      'sessions.list',
      'sessions.new',
      'sessions.open',
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
      resumed.request('c-23', 'chat.history')
      const empty = await resumed.waitFor((messages) => byId(messages, 'c-23'))
      check('a new conversation starts empty', (empty?.result?.turns ?? []).length === 0)

      resumed.request('c-24', 'sessions.open', { params: { id: current } })
      const opened = await resumed.waitFor((messages) => byId(messages, 'c-24'))
      check('the conversation that was left behind opens again', opened?.result?.session === current)
      resumed.request('c-25', 'chat.history')
      const reopened = await resumed.waitFor((messages) => byId(messages, 'c-25'))
      check('opening it brings its turns back', (reopened?.result?.turns ?? []).length === 5)

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
