# KeyForge

Lua-scriptable input automation for Android. Intercept, transform, and emit
evdev events at the kernel level — any key, any axis, any device.

Like AutoHotkey, but runs as an [AX Manager](https://github.com/ktanon/AX-Manager) module
with direct access to `/dev/input`.

## How It Works

1. **Your physical device is detected** — KeyForge grabs it exclusively via `/dev/input`
2. **After 1s, KeyForge creates a virtual controller** via uinput — mirrors your physical device's buttons and axes
3. **Raw input goes through the Lua pipeline** — modify stick curves, apply deadzones, remap buttons
4. **Transformed output goes to the virtual device** — any app sees it as real controller input

## Features

- **Lua pipeline** — chain plugins that process stick, trigger, and button events
- **Plugin API** — `pf.emit(type, code, value)`, `pf.drop()`, `pf.log()` for full control
- **Device mirroring** — copies physical device capabilities (keys, axes, absinfo) to virtual device
- **WebUI** — device selector, plugin manager, per-plugin settings with live reload
- **Hot reload** — config changes detected within 500ms, no restart needed
- **Per-plugin config** — settings saved to `/sdcard/.keyforge/configs/<id>.conf`
- **AX Manager module** — no APK needed, install as zip via AX Manager

## Install

1. Install [AX Manager](https://github.com/ktanon/AX-Manager)
2. Download `keyforge.zip` from [Releases](https://github.com/yoro1836/keyforge/releases)
3. Import as module in AX Manager
4. Start the module, open WebUI
5. Select your controller from the device dropdown
6. Add plugins from [keyforge-plugins](https://github.com/yoro1836/keyforge-plugins)

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

### pf API

| Function | Description |
|----------|-------------|
| `pf.emit(type, code, value [, hold_ms])` | Emit an evdev event |
| `pf.drop()` | Suppress the current event |
| `pf.log(msg)` | Write to daemon log |
| `pf.EV_KEY` / `pf.EV_ABS` | Event type constants |
| `pf.version` | API version string |
| `pf.raw_x` / `pf.raw_y` | Raw stick values before processing |

### Settings

Plugins declare settings in their metadata. The WebUI renders toggles, sliders, and number inputs
automatically. Supported kinds: `"toggle"`, `"permille"` (0-1000‰ with slider), `"number"`.

## Structure

```
module/            AX Manager module files
  keyforge.sh      Control script (start/stop/status/config/plugins/devices)
  webroot/
    index.html     Single-file WebUI (device selector, plugin manager, settings)
  module.prop      AX Manager module manifest
  service.sh       Module entry point
daemon/            Rust daemon (evdev → pipeline → uinput)
  src/
    main.rs        Event loop (epoll, config polling, inotify hotplug)
    core.rs        FFI, ioctl, Device, uinput setup
    pipeline.rs    Event types, pipeline, Processor trait, EmitEvent
    plugin/
      mod.rs       Lua plugin loader, LuaProcessor
      api.rs       pf API (emit, drop, log)
    config.rs      Config loader (key=value format)
```

## Data

All user data under `/sdcard/.keyforge/`:
```
/sdcard/.keyforge/
  manifest.json         Plugin manifest (auto-generated)
  plugins/*.lua         Installed plugin files
  configs/<id>.conf     Per-plugin settings
  keyforge.conf         Main config (VID, PID, plugin_dir)
```

## Build

```sh
# daemon (ARM64 Android)
cd daemon
cargo build --release --target aarch64-linux-android

# module zip
cd module
zip keyforge.zip module.prop service.sh keyforge.sh keyforge webroot/index.html
```

## License

MIT — see [LICENSE](LICENSE)
