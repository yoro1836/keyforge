import { exec as kernelSuExec, toast as kernelSuToast } from 'kernelsu'

const SHEVERY_STAGED_DIR = '/data/local/tmp/keyforge'
const STAGED_SCRIPT = `${SHEVERY_STAGED_DIR}/keyforge.sh`
const ROOT_MODULE_SCRIPT = '/data/adb/modules/keyforge/keyforge.sh'
// Base64 chars per bridge stdin call (stdin caps at 64KB).
const SHEVERY_STDIN_CHUNK = 49152
const AX_MODULE_SCRIPTS = [
  '/data/user_de/0/com.android.shell/axeron/plugins/keyforge/keyforge.sh',
  '/data/user_de/0/android/axeron/plugins/keyforge/keyforge.sh',
]

function normalizeResult(result) {
  if (typeof result === 'string') {
    return { errno: 0, stdout: result, stderr: '' }
  }
  if (result && typeof result === 'object') {
    return {
      errno: Number(result.errno ?? result.code ?? 0),
      stdout: String(result.stdout ?? result.out ?? ''),
      stderr: String(result.stderr ?? result.error ?? ''),
    }
  }
  return { errno: 0, stdout: result == null ? '' : String(result), stderr: '' }
}

export function shellQuote(value) {
  return `'${String(value).replace(/'/g, `'\\''`)}'`
}

export function escapeAxManagerCommand(command) {
  // AxManager currently transports commands by interpolating them into
  // `sh -c "..."`. Escape the outer shell's metacharacters so quotes and
  // expansions arrive unchanged at the inner shell.
  return String(command).replace(/[\\"$`]/g, '\\$&')
}

export function isAxManagerBridge() {
  return Boolean(globalThis.Axeron && typeof globalThis.Axeron.exec === 'function')
}
export function isShizukuBridge() {
  return Boolean(globalThis.Shizuku && typeof globalThis.Shizuku.exec === 'function')
}

export function hasCommandBridge() {
  return Boolean(
    isShizukuBridge() ||
      isAxManagerBridge() ||
      (globalThis.kernelsu && typeof globalThis.kernelsu.exec === 'function') ||
      (globalThis.ksu && typeof globalThis.ksu.exec === 'function'),
  )
}
export function parseShizukuResult(raw) {
  if (typeof raw === 'string') {
    try {
      raw = JSON.parse(raw)
    } catch {
      return { ok: true, exitCode: 0, stdout: raw, stderr: '', timedOut: false }
    }
  }
  if (raw && typeof raw === 'object') {
    const timedOut = raw.timedOut === true
    const ok = raw.ok === true && !timedOut
    return {
      ok,
      exitCode: Number(raw.exitCode ?? (ok ? 0 : 1)),
      stdout: String(raw.stdout ?? ''),
      stderr: timedOut && !raw.stderr ? 'Command timed out' : String(raw.stderr ?? ''),
      timedOut,
    }
  }
  return { ok: false, exitCode: 1, stdout: '', stderr: 'Empty shell result', timedOut: false }
}

export async function execRoot(command) {
  if (isShizukuBridge()) {
    // shevery API: exec returns JSON {ok, exitCode, stdout, stderr, timedOut}.
    const parsed = parseShizukuResult(await globalThis.Shizuku.exec(command))
    if (!parsed.ok) {
      throw new Error(parsed.stderr || `Command failed with code ${parsed.exitCode}`)
    }
    return parsed.stdout
  }
  let raw
  if (globalThis.kernelsu && typeof globalThis.kernelsu.exec === 'function') {
    raw = await globalThis.kernelsu.exec(command)
  } else if (globalThis.ksu && typeof globalThis.ksu.exec === 'function') {
    raw = await kernelSuExec(command)
  } else if (isAxManagerBridge()) {
    const response = globalThis.Axeron.exec(command, '{}')
    try {
      raw = JSON.parse(response)
    } catch {
      raw = response
    }
  } else {
    throw new Error('WebUI command bridge unavailable. Open this page from AX Manager, KernelSU, or shevery.')
  }

  const result = normalizeResult(raw)
  if (result.errno !== 0) {
    throw new Error(result.stderr.trim() || `Command failed with code ${result.errno}`)
  }
  return result.stdout
}

async function execWithStdin(command, stdin) {
  if (!isShizukuBridge() || typeof globalThis.Shizuku.execWithOptions !== 'function') {
    throw new Error('Shell stdin bridge unavailable.')
  }
  const parsed = parseShizukuResult(
    await globalThis.Shizuku.execWithOptions(command, JSON.stringify({ stdin })),
  )
  if (!parsed.ok) {
    throw new Error(parsed.stderr || `Command failed with code ${parsed.exitCode}`)
  }
  return parsed.stdout
}

export function base64Chunks(bytes, chunkChars = SHEVERY_STDIN_CHUNK) {
  const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes)
  const words = []
  const STEP = 0x8000
  for (let i = 0; i < view.length; i += STEP) {
    words.push(String.fromCharCode.apply(null, view.subarray(i, i + STEP)))
  }
  const encoded = btoa(words.join(''))
  const chunks = []
  for (let i = 0; i < encoded.length; i += chunkChars) {
    chunks.push(encoded.slice(i, i + chunkChars))
  }
  return chunks
}

// Read a module-local file through the WebView. Only possible under Full
// Trust (allowFileAccessFromFileURLs), which is our shevery baseline.
export async function fetchModuleFile(relativePath, { binary = false, baseHref = null } = {}) {
  const base = baseHref ?? globalThis.location?.href ?? null
  if (!base) {
    throw new Error('Cannot resolve module file URL.')
  }
  let response
  try {
    response = await fetch(new URL(relativePath, base))
  } catch (error) {
    throw new Error(
      'Cannot read module files. Enable Full Trust for KeyForge in shevery (long-press the module card → Trust), then reopen.',
    )
  }
  if (!response.ok) {
    throw new Error(`Cannot read module file ${relativePath} (HTTP ${response.status}).`)
  }
  return binary ? new Uint8Array(await response.arrayBuffer()) : await response.text()
}

async function writeStagedBytes(dest, bytes, { executable = false, onProgress = null, label = '' } = {}) {
  const chunks = base64Chunks(bytes)
  if (chunks.length === 0) {
    await execRoot(`: > ${dest}`)
  }
  for (let i = 0; i < chunks.length; i++) {
    await execWithStdin(`base64 -d ${i === 0 ? '>' : '>>'} ${dest}`, chunks[i])
    onProgress?.(label, i + 1, chunks.length)
  }
  if (executable) {
    await execRoot(`chmod 755 ${dest}`)
  }
}

// Ensure the staged runtime exists and matches the installed module version.
// File fetches require Full Trust; everything else is plain shell.
export async function ensureSheveryRuntime({ onProgress = null, baseHref = null } = {}) {
  let wanted = ''
  try {
    wanted = String(JSON.parse(globalThis.Shizuku.getModuleInfo())?.version ?? '')
  } catch (error) {
    throw new Error('Cannot read module info from shevery.')
  }
  let current = null
  try {
    current = (await execRoot(`cat ${SHEVERY_STAGED_DIR}/.version`)).trim()
  } catch {
    current = null
  }
  if (current !== null && current !== '' && current === wanted) {
    return { staged: false, version: wanted }
  }
  onProgress?.('script', 0, 1)
  await execRoot(`mkdir -p ${SHEVERY_STAGED_DIR}`)
  const script = await fetchModuleFile('../keyforge.sh', { baseHref })
  await writeStagedBytes(
    `${SHEVERY_STAGED_DIR}/keyforge.sh`,
    new TextEncoder().encode(script),
    { onProgress, label: 'script' },
  )
  const binary = await fetchModuleFile('../keyforge', { binary: true, baseHref })
  await writeStagedBytes(`${SHEVERY_STAGED_DIR}/keyforge`, binary, {
    executable: true,
    onProgress,
    label: 'daemon',
  })
  await execRoot(`printf '%s' ${shellQuote(wanted)} > ${SHEVERY_STAGED_DIR}/.version`)
  return { staged: true, version: wanted }
}

export function buildScriptCommand(args, axManager = isAxManagerBridge()) {
  const suffix = args.length ? ` ${args.map(shellQuote).join(' ')}` : ''
  const missing = `echo 'KeyForge module script not found' >&2; exit 127`

  if (!axManager && isShizukuBridge()) {
    // The module directory is unreadable from shell contexts, so the daemon
    // keeps a staged copy of the runtime here (see service.sh bootstrap).
    return `sh ${STAGED_SCRIPT}${suffix}`
  }
  if (!axManager) {
    return `if [ -f ${ROOT_MODULE_SCRIPT} ]; then sh ${ROOT_MODULE_SCRIPT}${suffix}; else ${missing}; fi`
  }

  const [shellScript, rootScript] = AX_MODULE_SCRIPTS
  const command = `if [ -f ${shellScript} ]; then AXERON=true sh ${shellScript}${suffix}; elif [ -f ${rootScript} ]; then AXERON=true sh ${rootScript}${suffix}; else ${missing}; fi`
  return escapeAxManagerCommand(command)
}

export function runScript(...args) {
  return execRoot(buildScriptCommand(args))
}

export function nativeToast(message) {
  try {
    if (globalThis.ksu && typeof globalThis.ksu.toast === 'function') {
      kernelSuToast(message)
    } else if (globalThis.kernelsu && typeof globalThis.kernelsu.toast === 'function') {
      globalThis.kernelsu.toast(message)
    } else if (globalThis.Axeron && typeof globalThis.Axeron.toast === 'function') {
      globalThis.Axeron.toast(message)
    }
  } catch {
    // The in-page snackbar remains the source of truth.
  }
}
