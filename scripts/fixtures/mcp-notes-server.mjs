#!/usr/bin/env node
// A minimal MCP server over stdio, used by the desktop bridge smoke test as a
// stand-in for a plugin a workspace declares in `plugins.json`.
//
// It speaks the same protocol a real server does: one JSON-RPC object per line
// on stdin, one per line on stdout. Anything it wants to say to a human goes to
// stderr, because stdout is the wire.

import { createInterface } from 'node:readline'

const TOOLS = [
  {
    name: 'add_note',
    description: 'Keep a note for later.',
    inputSchema: {
      type: 'object',
      properties: { text: { type: 'string' } },
      required: ['text'],
    },
  },
  {
    name: 'list_notes',
    description: 'Every note kept so far.',
    inputSchema: { type: 'object', properties: {} },
  },
]

const notes = []

function reply(id, result) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: '2.0', id, result })}\n`)
}

function fail(id, message) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: '2.0', id, error: { code: -32000, message } })}\n`)
}

createInterface({ input: process.stdin }).on('line', (line) => {
  if (line.trim() === '') return
  let message
  try {
    message = JSON.parse(line)
  } catch {
    return
  }
  const { id, method, params } = message
  if (id === undefined) return // a notification
  switch (method) {
    case 'initialize':
      reply(id, {
        protocolVersion: '2024-11-05',
        capabilities: { tools: {} },
        serverInfo: { name: 'notes', version: '0.0.1' },
      })
      break
    case 'tools/list':
      reply(id, { tools: TOOLS })
      break
    case 'tools/call': {
      const name = params?.name
      if (name === 'add_note') {
        const text = String(params?.arguments?.text ?? '')
        notes.push(text)
        reply(id, { content: [{ type: 'text', text: `kept ${notes.length} note(s)` }] })
      } else if (name === 'list_notes') {
        reply(id, {
          content: [{ type: 'text', text: notes.length ? notes.join('\n') : 'no notes yet' }],
        })
      } else {
        reply(id, {
          isError: true,
          content: [{ type: 'text', text: `unknown tool ${name}` }],
        })
      }
      break
    }
    default:
      fail(id, `unknown method ${method}`)
  }
})

process.stderr.write('notes mcp server ready\n')
