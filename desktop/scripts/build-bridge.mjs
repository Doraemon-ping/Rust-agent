#!/usr/bin/env node
// Build the rust-agent desktop bridge and stage it as a Tauri sidecar.
//
// Tauri's bundler expects an external binary at
//   src-tauri/binaries/<name>-<target-triple><.exe>
// so this script builds the sibling cargo binary and copies it there under the
// host triple. It only ever builds the host triple: staging a Windows binary as
// a macOS artifact would be a lie the bundler cannot detect.
//
//   node scripts/build-bridge.mjs            # debug
//   node scripts/build-bridge.mjs --release  # release

import { execFileSync } from 'node:child_process'
import { copyFileSync, mkdirSync, statSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import process from 'node:process'

const here = dirname(fileURLToPath(import.meta.url))
const desktop = resolve(here, '..')
const repo = resolve(desktop, '..')

const BIN_NAME = 'desktop_bridge'
const SIDECAR_STEM = 'rust-agent-bridge'

const release = process.argv.includes('--release')
const profile = release ? 'release' : 'debug'

function hostTriple() {
  // Ask rustc rather than guessing from process.platform/arch: the triple is
  // what cargo actually named the artifact after.
  const info = execFileSync('rustc', ['-vV'], { encoding: 'utf8' })
  const line = info.split(/\r?\n/).find((row) => row.startsWith('host:'))
  if (!line) throw new Error('rustc -vV did not report a host triple')
  return line.slice('host:'.length).trim()
}

const triple = hostTriple()
const exeSuffix = triple.includes('windows') ? '.exe' : ''

console.log(`[bridge] building ${BIN_NAME} (${profile}) for ${triple}`)
execFileSync('cargo', ['build', '--bin', BIN_NAME, ...(release ? ['--release'] : [])], {
  cwd: repo,
  stdio: 'inherit',
})

const built = join(repo, 'target', profile, `${BIN_NAME}${exeSuffix}`)
statSync(built) // fail loudly if cargo did not produce it

const outDir = join(desktop, 'src-tauri', 'binaries')
mkdirSync(outDir, { recursive: true })
const staged = join(outDir, `${SIDECAR_STEM}-${triple}${exeSuffix}`)
copyFileSync(built, staged)

console.log(`[bridge] staged ${built}`)
console.log(`[bridge]      -> ${staged}`)
