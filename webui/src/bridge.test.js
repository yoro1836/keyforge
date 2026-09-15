import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  buildScriptCommand,
  escapeAxManagerCommand,
  execRoot,
  hasCommandBridge,
  shellQuote,
} from './bridge.js'

test('shellQuote preserves apostrophes', () => {
  assert.equal(shellQuote("it's ready"), "'it'\\''s ready'")
})

test('AxManager command escaping survives its nested sh wrapper', () => {
  const argument = 'runtime "$PATH" `tick` \\ slash it\'s'
  const innerCommand = `printf '%s\\n' ${shellQuote(argument)}`
  const outerCommand = `sh -c "${escapeAxManagerCommand(innerCommand)}"`
  const result = spawnSync('sh', ['-c', outerCommand], { encoding: 'utf8' })

  assert.equal(result.status, 0, result.stderr)
  assert.equal(result.stdout, `${argument}\n`)
})

test('buildScriptCommand runs runtime as an argument through AxManager', () => {
  const directory = mkdtempSync(join(tmpdir(), 'keyforge-bridge-'))
  const script = join(directory, 'keyforge.sh')
  writeFileSync(script, '#!/bin/sh\nprintf "argument=<%s>\\n" "$1"\n')
  chmodSync(script, 0o755)

  try {
    const axCommand = buildScriptCommand(['runtime'], true).replace(
      /\/data\/user_de\/0\/(?:com\.android\.shell|android)\/axeron\/plugins\/keyforge\/keyforge\.sh/g,
      script,
    )
    const outerCommand = `sh -c "${axCommand}"`
    const result = spawnSync('sh', ['-c', outerCommand], { encoding: 'utf8' })

    assert.equal(result.status, 0, result.stderr)
    assert.equal(result.stdout, 'argument=<runtime>\n')
  } finally {
    rmSync(directory, { recursive: true, force: true })
  }
})

test('native AxManager bridge works without the KernelSU compatibility bridge', async () => {
  const previousAxeron = globalThis.Axeron
  const previousKsu = globalThis.ksu
  const previousKernelSu = globalThis.kernelsu
  delete globalThis.ksu
  delete globalThis.kernelsu
  globalThis.Axeron = {
    exec: (command, options) => {
      assert.equal(command, 'status')
      assert.equal(options, '{}')
      return JSON.stringify({ errno: 0, stdout: 'running pid=42\n', stderr: '' })
    },
  }

  try {
    assert.equal(hasCommandBridge(), true)
    assert.equal(await execRoot('status'), 'running pid=42\n')
  } finally {
    if (previousAxeron === undefined) delete globalThis.Axeron
    else globalThis.Axeron = previousAxeron
    if (previousKsu === undefined) delete globalThis.ksu
    else globalThis.ksu = previousKsu
    if (previousKernelSu === undefined) delete globalThis.kernelsu
    else globalThis.kernelsu = previousKernelSu
  }
})

test('buildScriptCommand uses manager-specific module locations', () => {
  const axCommand = buildScriptCommand(['runtime'], true)
  assert.match(axCommand, /com\.android\.shell\/axeron\/plugins\/keyforge/)
  assert.match(axCommand, /user_de\/0\/android\/axeron\/plugins\/keyforge/)
  assert.doesNotMatch(axCommand, /\$_kf/)
  assert.equal((axCommand.match(/'runtime'/g) || []).length, 2)

  const rootCommand = buildScriptCommand(['runtime'], false)
  assert.match(rootCommand, /\/data\/adb\/modules\/keyforge\/keyforge\.sh 'runtime'/)
  assert.doesNotMatch(rootCommand, /axeron\/plugins/)
})

function withShizuku(shizuku, fn) {
  const previous = {
    Shizuku: globalThis.Shizuku,
    Axeron: globalThis.Axeron,
    ksu: globalThis.ksu,
    kernelsu: globalThis.kernelsu,
  }
  delete globalThis.Axeron
  delete globalThis.ksu
  delete globalThis.kernelsu
  if (shizuku === undefined) delete globalThis.Shizuku
  else globalThis.Shizuku = shizuku
  return Promise.resolve()
    .then(fn)
    .finally(() => {
      for (const [key, value] of Object.entries(previous)) {
        if (value === undefined) delete globalThis[key]
        else globalThis[key] = value
      }
    })
}

test('parseShizukuResult handles shevery result shapes', async () => {
  const { parseShizukuResult } = await import('./bridge.js')
  assert.deepEqual(parseShizukuResult(JSON.stringify({ ok: true, exitCode: 0, stdout: 'hi\n', stderr: '', timedOut: false })), {
    ok: true, exitCode: 0, stdout: 'hi\n', stderr: '', timedOut: false,
  })
  const failed = parseShizukuResult(JSON.stringify({ ok: false, exitCode: 1, stdout: '', stderr: 'nope', timedOut: false }))
  assert.equal(failed.ok, false)
  assert.equal(failed.stderr, 'nope')
  const timedOut = parseShizukuResult(JSON.stringify({ ok: false, exitCode: 124, stdout: '', stderr: '', timedOut: true }))
  assert.equal(timedOut.ok, false)
  assert.match(timedOut.stderr, /timed out/i)
  assert.equal(parseShizukuResult('plain output').stdout, 'plain output')
})

test('Shizuku bridge executes and reports errors', async () => {
  const { execRoot, hasCommandBridge } = await import('./bridge.js')
  await withShizuku(
    {
      exec: (command) => {
        assert.equal(command, 'status')
        return JSON.stringify({ ok: true, exitCode: 0, stdout: 'running pid=7\n', stderr: '', timedOut: false })
      },
    },
    async () => {
      assert.equal(hasCommandBridge(), true)
      assert.equal(await execRoot('status'), 'running pid=7\n')
    },
  )
  await withShizuku(
    {
      exec: () => JSON.stringify({ ok: false, exitCode: -1, stdout: '', stderr: 'bridge blocked', timedOut: false }),
    },
    async () => {
      await assert.rejects(execRoot('status'), /bridge blocked/)
    },
  )
})

test('buildScriptCommand uses the staged runtime under Shizuku', async () => {
  const { buildScriptCommand } = await import('./bridge.js')
  await withShizuku(
    {
      exec: () => '{}',
    },
    async () => {
      assert.equal(
        buildScriptCommand(['status'], false),
        `sh /data/local/tmp/keyforge/keyforge.sh 'status'`,
      )
      // Explicit AxManager routing still wins when forced.
      assert.match(buildScriptCommand(['status'], true), /axeron\/plugins/)
    },
  )
})

test('base64Chunks round-trips binary data', async () => {
  const { base64Chunks } = await import('./bridge.js')
  const bytes = new Uint8Array([0, 1, 2, 250, 255, 65, 66, 67])
  const joined = base64Chunks(bytes, 4).join('')
  assert.equal(Buffer.from(joined, 'base64').toString('hex'), Buffer.from(bytes).toString('hex'))
  assert.deepEqual(base64Chunks(new Uint8Array(0)), [])
  const many = base64Chunks(new Uint8Array(100).fill(7), 10)
  assert.ok(many.length > 1)
  assert.ok(many.every((chunk) => chunk.length <= 10))
})

test('fetchModuleFile reports trust problems clearly', async () => {
  const { fetchModuleFile } = await import('./bridge.js')
  const previousFetch = globalThis.fetch
  try {
    globalThis.fetch = async () => ({
      ok: true, status: 200, text: async () => 'script-body', arrayBuffer: async () => new Uint8Array([9]).buffer,
    })
    assert.equal(await fetchModuleFile('../keyforge.sh', { baseHref: 'file:///m/webroot/index.html' }), 'script-body')
    const binary = await fetchModuleFile('../keyforge', { binary: true, baseHref: 'file:///m/webroot/index.html' })
    assert.ok(binary instanceof Uint8Array)
    assert.equal(binary[0], 9)
    globalThis.fetch = async () => ({ ok: false, status: 404 })
    await assert.rejects(fetchModuleFile('../x', { baseHref: 'file:///m/' }), /HTTP 404/)
    globalThis.fetch = async () => { throw new TypeError('Failed to fetch') }
    await assert.rejects(fetchModuleFile('../x', { baseHref: 'file:///m/' }), /Full Trust/)
  } finally {
    if (previousFetch === undefined) delete globalThis.fetch
    else globalThis.fetch = previousFetch
  }
})

test('ensureSheveryRuntime stages once then skips', async () => {
  const { ensureSheveryRuntime } = await import('./bridge.js')
  const previousFetch = globalThis.fetch
  const calls = []
  const shizuku = {
    getModuleInfo: () => JSON.stringify({ id: 'keyforge', version: 'v0-test' }),
    exec: (command) => {
      calls.push({ command, stdin: null })
      if (command.startsWith('cat ')) {
        return JSON.stringify({ ok: false, exitCode: 1, stdout: '', stderr: 'missing', timedOut: false })
      }
      return JSON.stringify({ ok: true, exitCode: 0, stdout: '', stderr: '', timedOut: false })
    },
    execWithOptions: (command, options) => {
      calls.push({ command, stdin: JSON.parse(options).stdin })
      return JSON.stringify({ ok: true, exitCode: 0, stdout: '', stderr: '', timedOut: false })
    },
  }
  globalThis.fetch = async (url) => {
    const path = String(url)
    if (path.endsWith('/keyforge.sh')) {
      return { ok: true, status: 200, text: async () => '#!/system/bin/sh\necho hi\n' }
    }
    return { ok: true, status: 200, arrayBuffer: async () => new TextEncoder().encode('FAKE-BINARY').buffer }
  }
  try {
    await withShizuku(shizuku, async () => {
      const first = await ensureSheveryRuntime({ baseHref: 'file:///m/webroot/index.html' })
      assert.equal(first.staged, true)
      const writes = calls.filter((call) => call.stdin !== null)
      assert.ok(writes.length >= 2)
      assert.match(writes[0].command, /> \/data\/local\/tmp\/keyforge\/keyforge\.sh$/)
      assert.ok(calls.some((call) => /chmod 755 \/data\/local\/tmp\/keyforge\/keyforge$/.test(call.command)))
      assert.ok(calls.some((call) => call.command.includes('.version')))
      // Second run sees the staged version and performs no writes.
      calls.length = 0
      shizuku.exec = (command) => JSON.stringify({
        ok: true, exitCode: 0,
        stdout: command.startsWith('cat ') ? 'v0-test\n' : '',
        stderr: '', timedOut: false,
      })
      const second = await ensureSheveryRuntime({ baseHref: 'file:///m/webroot/index.html' })
      assert.equal(second.staged, false)
      assert.ok(calls.every((call) => call.stdin === null))
    })
  } finally {
    if (previousFetch === undefined) delete globalThis.fetch
    else globalThis.fetch = previousFetch
  }
})
