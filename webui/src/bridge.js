import { exec as kernelSuExec, toast as kernelSuToast } from 'kernelsu'

const STAGED_SCRIPT = '/data/local/tmp/keyforge/keyforge.sh'
const ROOT_MODULE_SCRIPT = '/data/adb/modules/keyforge/keyforge.sh'
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
