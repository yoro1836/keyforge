<script setup>
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { hasCommandBridge, nativeToast, runScript } from './bridge.js'

const KEY_PATTERN = /^[A-Za-z0-9._-]+$/

const initialLoading = ref(true)
const bridgeError = ref('')
const daemonState = ref('checking')
const daemonBusy = ref(false)
const runtime = ref({
  framework: 'unknown',
  deviceHideSupported: false,
  deviceHideEnabled: false,
  deviceHideActive: false,
})
const devices = ref([])
const selectedDevice = ref('')
const deviceBusy = ref(false)
const hideBusy = ref(false)
const plugins = ref([])
const pluginConfigs = ref({})
const installedPlugins = ref([])
const pluginBusy = ref({})
const dirty = ref(false)
const saving = ref(false)
const refreshing = ref(false)
const logOpen = ref(false)
const logText = ref('')
const snackbar = ref({ visible: false, message: '' })
let snackbarTimer
let statusTimer
let refreshTimer

const frameworkLabel = computed(() => {
  const labels = {
    kernelsu: 'KernelSU',
    magisk: 'Magisk',
    axmanager: 'AX Manager',
    shizuku: 'shevery',
    unknown: 'Unknown manager',
  }
  return labels[runtime.value.framework] || runtime.value.framework
})

const selectedDetails = computed(() =>
  devices.value.find((device) => deviceKey(device) === selectedDevice.value),
)

const loadedPluginIds = computed(() => new Set(plugins.value.map((plugin) => plugin.id)))
const showDeviceHide = computed(() => runtime.value.deviceHideSupported)

function parseJson(raw, fallback) {
  try {
    return JSON.parse(raw)
  } catch {
    return fallback
  }
}

function deviceKey(device) {
  return `${String(device.vid).toLowerCase()}:${String(device.pid).toLowerCase()}`
}

function showMessage(message) {
  const text = String(message || '').trim() || 'Done'
  snackbar.value = { visible: true, message: text }
  nativeToast(text)
  clearTimeout(snackbarTimer)
  snackbarTimer = setTimeout(() => {
    snackbar.value.visible = false
  }, 2600)
}

function showError(error) {
  showMessage(error instanceof Error ? error.message : String(error))
}

async function loadRuntime() {
  runtime.value = parseJson(await runScript('runtime'), runtime.value)
}

async function checkStatus() {
  const output = (await runScript('status')).trim()
  daemonState.value = output.startsWith('running') ? 'running' : 'stopped'
}

async function loadDevices() {
  const [deviceOutput, vid, pid] = await Promise.all([
    runScript('devices'),
    runScript('config', 'get', 'VID'),
    runScript('config', 'get', 'PID'),
  ])
  devices.value = parseJson(deviceOutput, { devices: [] }).devices || []
  selectedDevice.value = `${vid.trim().toLowerCase()}:${pid.trim().toLowerCase()}`
}

function parseConfig(raw) {
  const values = {}
  for (const line of raw.split('\n')) {
    const split = line.indexOf('=')
    if (split > 0) {
      values[line.slice(0, split)] = line.slice(split + 1)
    }
  }
  return values
}

async function loadManifest() {
  const manifest = parseJson(await runScript('manifest'), { plugins: [] })
  const nextPlugins = Array.isArray(manifest.plugins) ? manifest.plugins : []
  const entries = await Promise.all(
    nextPlugins.map(async (plugin) => {
      if (!KEY_PATTERN.test(plugin.id)) return [plugin.id, {}]
      try {
        return [plugin.id, parseConfig(await runScript('plugins', 'config', plugin.id))]
      } catch {
        return [plugin.id, {}]
      }
    }),
  )
  plugins.value = nextPlugins
  pluginConfigs.value = Object.fromEntries(entries)
}

async function loadInstalledPlugins() {
  const result = parseJson(await runScript('plugins', 'list'), { plugins: [] })
  installedPlugins.value = Array.isArray(result.plugins) ? result.plugins : []
}

async function refreshAll({ quiet = false } = {}) {
  if (!hasCommandBridge()) {
    bridgeError.value = 'WebUI command bridge unavailable. Open KeyForge from AX Manager, KernelSU, or shevery.'
    initialLoading.value = false
    return
  }

  refreshing.value = !quiet
  bridgeError.value = ''
  const results = await Promise.allSettled([
    loadRuntime(),
    checkStatus(),
    loadDevices(),
    loadManifest(),
    loadInstalledPlugins(),
  ])
  const failure = results.find((result) => result.status === 'rejected')
  if (failure) {
    bridgeError.value = failure.reason?.message || String(failure.reason)
  }
  refreshing.value = false
  initialLoading.value = false
}

async function daemonAction(action) {
  daemonBusy.value = true
  try {
    const output = await runScript(action)
    showMessage(output.split('\n')[0])
    // The daemon double-forks and writes its pidfile before the script
    // reports success; a short wait keeps the status badge accurate.
    await new Promise((resolve) => setTimeout(resolve, 900))
    await Promise.all([checkStatus(), loadRuntime()])
  } catch (error) {
    showError(error)
  } finally {
    daemonBusy.value = false
  }
}

async function chooseDevice() {
  const device = selectedDetails.value
  if (!device) return
  deviceBusy.value = true
  try {
    await runScript('config', 'batch', `VID=${device.vid}`, `PID=${device.pid}`)
    showMessage(`Selected ${device.name}`)
    await daemonAction('restart')
  } catch (error) {
    showError(error)
  } finally {
    deviceBusy.value = false
  }
}

async function scanDevices() {
  deviceBusy.value = true
  try {
    await loadDevices()
    showMessage(`${devices.value.length} physical device${devices.value.length === 1 ? '' : 's'} found`)
  } catch (error) {
    showError(error)
  } finally {
    deviceBusy.value = false
  }
}

async function setDeviceHidden(event) {
  const enabled = event.target.checked
  if (!runtime.value.deviceHideSupported) return
  if (enabled && !selectedDetails.value) {
    event.target.checked = runtime.value.deviceHideEnabled
    showMessage('Select a source device before enabling device hiding')
    return
  }
  hideBusy.value = true
  try {
    await runScript('hide', enabled ? 'on' : 'off')
    runtime.value.deviceHideEnabled = enabled
    showMessage(enabled ? 'Physical device hiding enabled' : 'Physical device hiding disabled')
    await new Promise((resolve) => setTimeout(resolve, 700))
    await loadRuntime()
  } catch (error) {
    event.target.checked = runtime.value.deviceHideEnabled
    showError(error)
  } finally {
    hideBusy.value = false
  }
}

function settingValue(plugin, setting) {
  const saved = pluginConfigs.value[plugin.id]?.[setting.key]
  return saved == null ? String(setting.default ?? 0) : saved
}

function updateSetting(pluginId, key, value) {
  if (!pluginConfigs.value[pluginId]) pluginConfigs.value[pluginId] = {}
  pluginConfigs.value[pluginId][key] = String(value)
  dirty.value = true
}

function encodeBase64(text) {
  const bytes = new TextEncoder().encode(text)
  let binary = ''
  for (const byte of bytes) binary += String.fromCharCode(byte)
  return btoa(binary)
}

async function saveAll() {
  saving.value = true
  try {
    const writes = plugins.value
      .filter((plugin) => KEY_PATTERN.test(plugin.id))
      .map((plugin) => {
        const values = pluginConfigs.value[plugin.id] || {}
        const lines = (plugin.settings || [])
          .filter((setting) => KEY_PATTERN.test(setting.key))
          .map((setting) => `${setting.key}=${values[setting.key] ?? setting.default ?? 0}`)
        return runScript('plugins', 'save-config', plugin.id, encodeBase64(`${lines.join('\n')}\n`))
      })
    await Promise.all(writes)
    await runScript('config', 'set', '_reload', String(Date.now()))
    dirty.value = false
    showMessage('Plugin settings saved')
  } catch (error) {
    showError(error)
  } finally {
    saving.value = false
  }
}

async function togglePlugin(plugin, enabled) {
  pluginBusy.value = { ...pluginBusy.value, [plugin.id]: true }
  try {
    await runScript('plugins', enabled ? 'enable' : 'disable', plugin.id)
    plugin.enabled = enabled
    showMessage(`${plugin.name} ${enabled ? 'enabled' : 'disabled'}`)
    await new Promise((resolve) => setTimeout(resolve, 900))
    await Promise.all([checkStatus(), loadManifest()])
  } catch (error) {
    showError(error)
  } finally {
    pluginBusy.value = { ...pluginBusy.value, [plugin.id]: false }
  }
}

async function movePlugin(index, delta) {
  const target = index + delta
  if (target < 0 || target >= plugins.value.length) return
  const ids = plugins.value.map((plugin) => plugin.id)
  ;[ids[index], ids[target]] = [ids[target], ids[index]]
  try {
    await runScript('config', 'set', 'plugin_order', ids.join(','))
    plugins.value = ids
      .map((id) => plugins.value.find((plugin) => plugin.id === id))
      .filter(Boolean)
    showMessage('Plugin order updated')
    await new Promise((resolve) => setTimeout(resolve, 900))
    await loadManifest()
  } catch (error) {
    showError(error)
  }
}

async function uploadPlugin(event) {
  const input = event.target
  const file = input.files?.[0]
  if (!file) return
  try {
    if (!file.name.endsWith('.lua')) throw new Error('Choose a .lua plugin file')
    if (!KEY_PATTERN.test(file.name)) {
      throw new Error('Plugin filename may only contain letters, numbers, dot, dash, and underscore')
    }
    const bytes = new Uint8Array(await file.arrayBuffer())
    let binary = ''
    for (const byte of bytes) binary += String.fromCharCode(byte)
    await runScript('plugins', 'upload', file.name, btoa(binary))
    await runScript('config', 'set', '_reload', String(Date.now()))
    showMessage(`${file.name} installed`)
    await new Promise((resolve) => setTimeout(resolve, 700))
    await Promise.all([loadInstalledPlugins(), loadManifest()])
  } catch (error) {
    showError(error)
  } finally {
    input.value = ''
  }
}

async function removePlugin(id) {
  if (!KEY_PATTERN.test(id)) return
  pluginBusy.value = { ...pluginBusy.value, [id]: true }
  try {
    await runScript('plugins', 'remove', id)
    await runScript('config', 'set', '_reload', String(Date.now()))
    showMessage(`${id} removed`)
    await new Promise((resolve) => setTimeout(resolve, 700))
    await Promise.all([loadInstalledPlugins(), loadManifest()])
  } catch (error) {
    showError(error)
  } finally {
    pluginBusy.value = { ...pluginBusy.value, [id]: false }
  }
}

async function toggleLog() {
  logOpen.value = !logOpen.value
  if (!logOpen.value) return
  try {
    logText.value = (await runScript('log')).trim() || 'No log entries yet.'
  } catch (error) {
    logText.value = error instanceof Error ? error.message : String(error)
  }
}

onMounted(async () => {
  await refreshAll()
  statusTimer = setInterval(() => checkStatus().catch(() => {}), 8000)
  refreshTimer = setInterval(() => refreshAll({ quiet: true }), 30000)
})

onBeforeUnmount(() => {
  clearTimeout(snackbarTimer)
  clearInterval(statusTimer)
  clearInterval(refreshTimer)
})
</script>

<template>
  <main class="app-shell">
    <header class="hero surface-container-high">
      <div class="hero-orbit hero-orbit-one"></div>
      <div class="hero-orbit hero-orbit-two"></div>
      <div class="hero-copy">
        <div class="brand-row">
          <div class="brand-mark" aria-hidden="true"><span>K</span><span>F</span></div>
          <div>
            <p class="eyebrow">INPUT, REFORGED</p>
            <h1>KeyForge</h1>
          </div>
        </div>
        <p class="hero-subtitle">Shape physical input into a controller that feels entirely yours.</p>
        <div class="meta-row">
          <span class="status-pill" :class="`status-${daemonState}`">
            <span class="status-dot"></span>
            {{ daemonState }}
          </span>
          <span class="manager-pill">{{ frameworkLabel }}</span>
        </div>
      </div>

      <div class="hero-actions" aria-label="Daemon controls">
        <button class="button filled tonal-start" :disabled="daemonBusy" @click="daemonAction('start')">
          Start
        </button>
        <button class="button tonal" :disabled="daemonBusy" @click="daemonAction('restart')">
          Restart
        </button>
        <button class="button text-button danger" :disabled="daemonBusy" @click="daemonAction('stop')">
          Stop
        </button>
      </div>
    </header>

    <div v-if="bridgeError" class="error-banner" role="alert">
      <strong>Connection unavailable</strong>
      <span>{{ bridgeError }}</span>
    </div>

    <section v-if="initialLoading" class="loading-grid" aria-label="Loading KeyForge">
      <div v-for="index in 3" :key="index" class="skeleton-card"></div>
    </section>

    <template v-else>
      <section
        class="dashboard-grid"
        :class="{ 'single-column-lead': !showDeviceHide }"
        aria-label="KeyForge controls"
      >
        <article class="panel device-panel surface-container">
          <div class="panel-heading">
            <div>
              <p class="eyebrow">PHYSICAL INPUT</p>
              <h2>Source device</h2>
            </div>
            <button class="icon-text-button" :disabled="deviceBusy" @click="scanDevices">
              {{ deviceBusy ? 'Scanning' : 'Scan' }}
            </button>
          </div>

          <label class="field-label" for="device-select">Controller</label>
          <div class="select-wrap">
            <select id="device-select" v-model="selectedDevice" :disabled="deviceBusy" @change="chooseDevice">
              <option value="">No device selected</option>
              <option v-for="device in devices" :key="`${device.handler}-${deviceKey(device)}`" :value="deviceKey(device)">
                {{ device.name }} · {{ device.vid }}:{{ device.pid }}
              </option>
            </select>
          </div>

          <div v-if="selectedDetails" class="device-detail">
            <div class="device-glyph" aria-hidden="true">
              <span></span><span></span><span></span><span></span>
            </div>
            <div>
              <strong>{{ selectedDetails.name }}</strong>
              <p>{{ selectedDetails.handler }} · {{ selectedDetails.vid }}:{{ selectedDetails.pid }}</p>
            </div>
          </div>
          <p v-else class="supporting-text">
            Connect a controller, scan, then select the physical device to mirror.
          </p>
        </article>

        <article v-if="showDeviceHide" class="panel hide-panel" :class="{ 'hide-active': runtime.deviceHideActive }">
          <div class="panel-heading">
            <div>
              <p class="eyebrow">ROOT PRIVACY</p>
              <h2>Hide physical device</h2>
            </div>
            <label
              class="switch"
              :class="{ disabled: !runtime.deviceHideSupported || hideBusy || (!selectedDetails && !runtime.deviceHideEnabled) }"
            >
              <input
                type="checkbox"
                :checked="runtime.deviceHideEnabled"
                :disabled="!runtime.deviceHideSupported || hideBusy || (!selectedDetails && !runtime.deviceHideEnabled)"
                aria-label="Hide the selected physical device from Android"
                @change="setDeviceHidden"
              />
              <span class="switch-track"><span class="switch-thumb"></span></span>
            </label>
          </div>
          <div class="hide-target">
            <span>Target device</span>
            <strong>{{ selectedDetails?.name || selectedDevice || 'Select a source device above' }}</strong>
            <small v-if="selectedDetails">{{ selectedDetails.handler }} · {{ selectedDetails.vid }}:{{ selectedDetails.pid }}</small>
          </div>


          <div class="hide-visual" aria-hidden="true">
            <div class="device-node">event</div>
            <div class="hide-slash"></div>
            <div class="mode-chip">unlinked</div>
          </div>
          <p v-if="selectedDetails" class="supporting-text">
            The switch controls hiding for {{ selectedDetails.name }}. KeyForge moves its event node to a private link, so Android's EventHub unregisters the physical controller.
          </p>
          <p v-else-if="runtime.deviceHideEnabled" class="supporting-text">
            Hiding remains enabled for the configured source. Reconnect it and scan again, or turn the switch off to restore the node.
          </p>
          <p v-else class="supporting-text">
            Select the physical source device above, then enable this switch.
          </p>
          <span class="availability-chip">
            {{ runtime.deviceHideActive ? 'Physical node hidden' : runtime.deviceHideEnabled ? 'Applies when daemon connects' : 'Physical node visible' }}
          </span>
        </article>

        <article class="panel plugins-panel surface-container-low">
          <div class="panel-heading plugins-heading">
            <div>
              <p class="eyebrow">PROCESSING CHAIN</p>
              <h2>Lua plugins</h2>
              <p class="supporting-text">Tune every transformation, then save once.</p>
            </div>
            <div class="plugin-actions">
              <label class="button tonal upload-button">
                Add plugin
                <input type="file" accept=".lua" @change="uploadPlugin" />
              </label>
              <button class="button filled" :disabled="saving || !dirty" @click="saveAll">
                {{ saving ? 'Saving' : dirty ? 'Save changes' : 'Saved' }}
              </button>
            </div>
          </div>

          <div v-if="plugins.length" class="plugin-stack">
            <section
              v-for="(plugin, index) in plugins"
              :key="plugin.id"
              class="plugin-card"
              :class="{ 'plugin-disabled': !plugin.enabled }"
            >
              <div class="plugin-title-row">
                <div class="plugin-monogram" aria-hidden="true">{{ plugin.name.slice(0, 1).toUpperCase() }}</div>
                <div class="plugin-title">
                  <div class="title-line">
                    <h3>{{ plugin.name }}</h3>
                    <span>v{{ plugin.version }}</span>
                  </div>
                  <p>{{ plugin.description || `by ${plugin.author}` }}</p>
                </div>
                <div v-if="plugins.length > 1" class="order-buttons">
                  <button
                    class="order-button"
                    :disabled="index === 0 || pluginBusy[plugin.id]"
                    :aria-label="`Run ${plugin.name} earlier`"
                    @click="movePlugin(index, -1)"
                  >
                    ↑
                  </button>
                  <button
                    class="order-button"
                    :disabled="index === plugins.length - 1 || pluginBusy[plugin.id]"
                    :aria-label="`Run ${plugin.name} later`"
                    @click="movePlugin(index, 1)"
                  >
                    ↓
                  </button>
                </div>
                <label class="switch" :class="{ disabled: pluginBusy[plugin.id] }">
                  <input
                    type="checkbox"
                    :checked="plugin.enabled"
                    :disabled="pluginBusy[plugin.id]"
                    :aria-label="`${plugin.enabled ? 'Disable' : 'Enable'} ${plugin.name}`"
                    @change="togglePlugin(plugin, $event.target.checked)"
                  />
                  <span class="switch-track"><span class="switch-thumb"></span></span>
                </label>
              </div>

              <div v-if="plugin.settings?.length" class="settings-grid">
                <div v-for="setting in plugin.settings" :key="setting.key" class="setting-row">
                  <div class="setting-label">
                    <label :for="`${plugin.id}-${setting.key}`">{{ setting.label }}</label>
                    <span>{{ setting.key }}</span>
                  </div>

                  <template v-if="setting.kind === 'toggle'">
                    <label class="switch compact">
                      <input
                        :id="`${plugin.id}-${setting.key}`"
                        type="checkbox"
                        :checked="settingValue(plugin, setting) === '1'"
                        @change="updateSetting(plugin.id, setting.key, $event.target.checked ? '1' : '0')"
                      />
                      <span class="switch-track"><span class="switch-thumb"></span></span>
                    </label>
                  </template>

                  <template v-else-if="setting.kind === 'permille'">
                    <div class="range-control">
                      <input
                        :id="`${plugin.id}-${setting.key}`"
                        type="range"
                        :min="setting.min ?? 0"
                        :max="setting.max ?? 1000"
                        :value="settingValue(plugin, setting)"
                        @input="updateSetting(plugin.id, setting.key, $event.target.value)"
                      />
                      <input
                        class="number-field"
                        type="number"
                        :min="setting.min ?? 0"
                        :max="setting.max ?? 1000"
                        :value="settingValue(plugin, setting)"
                        :aria-label="`${setting.label} numeric value`"
                        @input="updateSetting(plugin.id, setting.key, $event.target.value)"
                      />
                    </div>
                  </template>

                  <input
                    v-else
                    :id="`${plugin.id}-${setting.key}`"
                    class="number-field"
                    type="number"
                    :min="setting.min"
                    :max="setting.max"
                    :value="settingValue(plugin, setting)"
                    @input="updateSetting(plugin.id, setting.key, $event.target.value)"
                  />
                </div>
              </div>
              <p v-else class="no-settings">No adjustable settings.</p>
            </section>
          </div>

          <div v-else class="empty-state">
            <div class="empty-shape" aria-hidden="true"></div>
            <h3>No active plugins yet</h3>
            <p>Add a Lua plugin to start shaping your input pipeline.</p>
          </div>

          <div v-if="installedPlugins.length" class="installed-row">
            <span class="field-label">Installed files</span>
            <div class="chip-row">
              <span v-for="id in installedPlugins" :key="id" class="plugin-chip">
                <span>{{ id }}</span>
                <span v-if="loadedPluginIds.has(id)" class="loaded-dot" title="Loaded"></span>
                <button :disabled="pluginBusy[id]" :aria-label="`Remove ${id}`" @click="removePlugin(id)">Remove</button>
              </span>
            </div>
          </div>
        </article>

        <article class="panel log-panel surface-container">
          <button class="log-toggle" :aria-expanded="logOpen" @click="toggleLog">
            <span>
              <span class="eyebrow">DIAGNOSTICS</span>
              <strong>Daemon log</strong>
            </span>
            <span class="disclosure" :class="{ open: logOpen }" aria-hidden="true"></span>
          </button>
          <pre v-if="logOpen" class="log-output">{{ logText }}</pre>
        </article>
      </section>
    </template>

    <button class="refresh-fab" :class="{ spinning: refreshing }" :disabled="refreshing" aria-label="Refresh all data" @click="refreshAll()">
      <span aria-hidden="true"></span>
      Refresh
    </button>

    <Transition name="snackbar">
      <div v-if="snackbar.visible" class="snackbar" role="status" aria-live="polite">
        {{ snackbar.message }}
      </div>
    </Transition>
  </main>
</template>
