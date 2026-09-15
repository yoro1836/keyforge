# KeyForge

Lua-scriptable input automation for rooted Android. Intercept, transform, and emit
evdev events at the kernel boundary — any key, any axis, any device.

KeyForge ships as one module ZIP for AX Manager, KernelSU, Magisk, and shevery, with
direct access to `/dev/input`.

## How It Works

1. **Your physical device is detected** — KeyForge grabs it exclusively via `/dev/input`
2. **After 1s, KeyForge creates a virtual controller** via UHID — mirrors your physical device's buttons and axes as a kernel HID device
3. **Raw input goes through the Lua pipeline** — modify stick curves, apply deadzones, remap buttons
4. **Transformed output goes to the virtual device** — any app sees it as real controller input

## Features

- **Lua pipeline** — chain plugins that process stick, trigger, and button events
- **Plugin API** — `pf.emit(type, code, value)`, `pf.drop()`, `pf.log()` for full control
- **Device mirroring** — builds a UHID HID mirror of the physical device's buttons and stick/trigger axes
- **Physical-device hiding** — KernelSU/Magisk can unlink the selected event node so Android unregisters the physical controller; AX Manager never exposes this control
- **Vue WebUI** — offline Vue 3 interface with a Material 3 Expressive design
- **Hot reload** — config changes detected within 500ms, no restart needed
- **Per-plugin config** — settings saved to `/sdcard/.keyforge/configs/<id>.conf`
- **Cross-manager module** — one ZIP supports AX Manager, KernelSU, Magisk, and shevery (Shizuku ADB modules)

## Install

### KernelSU or Magisk

1. Download `keyforge.zip` from [Releases](https://github.com/yoro1836/keyforge/releases).
2. Install the ZIP from the manager's Modules screen, then reboot.
3. In KernelSU, open KeyForge's WebUI and select a controller.
4. Add plugins from [keyforge-plugins](https://github.com/yoro1836/keyforge-plugins).

Magisk runs the same boot service and device-hiding implementation. Because the
official Magisk app does not host module WebUIs, open KeyForge through a
compatible Magisk module WebUI client; all configuration remains in the same UI.

### AX Manager

1. Import the same module ZIP in AX Manager.
2. Start the module, open its WebUI, and select a controller.

### shevery (Shizuku ADB modules)

1. Install the same module ZIP in shevery.
2. The WebUI talks through the Shizuku shell bridge (`usesShellBridge=true`);
   no remote assets are loaded.
3. In ADB-shell mode the daemon runs with shell privileges: `/dev/input`
   grab and `/dev/uhid` device creation work where the shell user belongs to
   the `input`/`uhid` groups, but device hiding needs root. In root mode
   everything works. Deleting the module makes the running daemon restore
   the physical device and exit by itself.

## Physical-device hiding

On KernelSU, Magisk, or root shevery, use the WebUI for the complete flow. The card is omitted
entirely in AX Manager because its ADB-level plugin environment must not rename
or remove `/dev/input` nodes.

1. Scan and select the controller under **Source device**.
2. Turn on **Hide physical device** for that selected controller.
3. The status chip confirms when its physical event node is hidden.

KeyForge opens and exclusively grabs the selected `/dev/input/event*` node,
hard-links it to a private `/dev/.keyforge-input-*` name, then removes the
original name. Android's EventHub receives the inotify removal event and
unregisters the physical controller, so apps only see the KeyForge virtual
controller. Disabling the option, stopping the daemon, starting after an
interrupted run, or uninstalling the module restores the original node (only
when the same device is still present). KeyForge also migrates state written by
older permission-based builds. AX Manager continues to use exclusive evdev
grabbing without changing device-node permissions.

## Plugin API

```lua
-- deadzone: zero small stick movements
return {
  id = "deadzone", name = "Deadzone", version = "1.0.0", author = "me",
  settings = {
    { key = "dz_left",  label = "Left (‰)",  kind = "permille", default = "91", min = 0, max = 1000 },
    { key = "dz_right", label = "Right (‰)", kind = "permille", default = "91", min = 0, max = 1000 },
  },
  process = function(ev, cfg, pf)
    if ev.kind ~= "stick" then return ev end
    local dz = tonumber(cfg["dz_" .. ev.side]) or 0
    if dz <= 0 then return ev end
    local thr = dz * 32767 / 1000
    if ev.x * ev.x + ev.y * ev.y < thr * thr then
      ev.x = 0; ev.y = 0
    end
    return ev
  end
}
```

### Event types

| `ev.kind` | Fields | Description |
|-----------|--------|-------------|
| `"stick"` | `x, y, side` | Analog stick (side = `"left"` or `"right"`) |
| `"trigger"` | `value, side` | Trigger (value = 0..32767) |
| `"button"` | `code, pressed` | Button (pressed = boolean) |

Plugins run as one chain and share a single event. `pf.emit` targeting the
current event's own axes or button replaces its values in place (so later
plugins see the result); `pf.drop` without a replacement suppresses the event.
Emits for other axes, buttons, or with `hold_ms` go straight to the virtual
device.

Chain order defaults to alphabetical by plugin id and can be rearranged in the
WebUI with the up/down buttons on each plugin card (stored as `plugin_order` in
the module config).

### pf API

| Function | Description |
|----------|-------------|
| `pf.emit(type, code, value [, hold_ms])` | Emit an evdev event |
| `pf.drop()` | Suppress the current event |
| `pf.log(msg)` | Write to daemon log |
| `pf.version` | API version string |
| `pf.raw_x` / `pf.raw_y` | Raw stick values before processing |

### Settings

Plugins declare settings in their metadata. The WebUI renders toggles, sliders, and number inputs
automatically. Supported kinds: `"toggle"`, `"permille"` (0-1000‰ with slider), `"number"`.

## Structure

```
module/            Installable AX Manager / KernelSU / Magisk / shevery module
  keyforge.sh      Control script and manager/runtime detection
  customize.sh     KernelSU/Magisk installer permissions and ABI check
  service.sh       Boot entry: daemon start plus detached request supervisor
  uninstall.sh     Supervisor, daemon, and hidden-device cleanup
  webroot/         Built, fully offline WebUI assets
webui/             Vue 3 + Vite WebUI source
  src/App.vue      Device, hiding, plugin, and daemon controls
  src/bridge.js    AX Manager and KernelSU command bridge adapter
daemon/            Rust daemon (evdev → pipeline → UHID)
  src/
    main.rs        Event loop, config polling, hotplug, visibility changes
    core.rs        FFI, ioctl, Device, device-node isolation and recovery
    uhid.rs        UHID C-API port, report builders, Lua `uh` table, mirror
    pipeline.rs    Event types, pipeline, Processor trait, EmitEvent
    plugin/
      mod.rs       Lua plugin loader, LuaProcessor
      api.rs       pf API (emit, drop, log)
    config.rs      Config loader (key=value format)
```

## Data

Plugin data is stored under `/sdcard/.keyforge/`:
```
/sdcard/.keyforge/
  manifest.json         Plugin manifest (auto-generated)
  plugins/*.lua         Installed plugin files
  configs/<id>.conf     Per-plugin settings
```

The main runtime config and hidden-device recovery state stay inside the module
directory so boot scripts can read them before shared storage is available.

## Build

```sh
# offline WebUI assets (writes module/webroot)
cd webui
npm ci
npm run build

# daemon (ARM64 Android)
cd ../daemon
cargo build --locked --release --target aarch64-linux-android

# module ZIP
cd ..
rm -rf pkg && mkdir pkg
cp -a module/. pkg/
cp daemon/target/aarch64-linux-android/release/keyforge pkg/
chmod 755 pkg/keyforge pkg/*.sh
(cd pkg && zip -r ../keyforge.zip .)
```

## License

MIT — see [LICENSE](LICENSE)
