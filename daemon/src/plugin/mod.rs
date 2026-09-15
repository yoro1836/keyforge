mod api;

use crate::core::PLUGIN_MANIFEST;
use crate::pipeline::{Ctx, EmitEvent, Event, Pipeline, Processor};
use mlua::{Function, Lua, Table, Value};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize)]
pub struct PluginMeta {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
    pub settings: Vec<SettingDef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingDef {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub default: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<i32>,
}

#[derive(Serialize)]
pub struct Manifest {
    pub plugins: Vec<PluginMeta>,
}

pub struct LuaProcessor {
    #[allow(dead_code)]
    pub meta: PluginMeta,
    lua: Lua,
    process_fn: Function,
    /// Per-plugin config loaded from /sdcard/.keyforge/configs/<id>.conf
    plugin_config: HashMap<String, String>,
}

impl Processor for LuaProcessor {
    fn id(&self) -> &str {
        &self.meta.id
    }

    fn process(&self, event: &mut Event, ctx: &mut Ctx) {
        let table = match build_event_table(&self.lua, event) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("keyforge[{}]: build_event_table: {}", self.meta.id, e);
                return;
            }
        };
        // Merge global config with per-plugin config (per-plugin takes priority)
        let mut merged = ctx.settings.clone();
        for (k, v) in &self.plugin_config {
            merged.insert(k.clone(), v.clone());
        }
        let cfg = match build_cfg_table(&self.lua, &merged) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("keyforge[{}]: build_cfg_table: {}", self.meta.id, e);
                return;
            }
        };

        let emits_buf: Arc<Mutex<Vec<EmitEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let drop_flag: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let raw_x = match event {
            Event::Stick { x, .. } => *x,
            _ => 0,
        };
        let raw_y = match event {
            Event::Stick { y, .. } => *y,
            _ => 0,
        };
        let pf = match api::build_pf(&self.lua, &emits_buf, &drop_flag, raw_x, raw_y) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("keyforge[{}]: build_pf: {}", self.meta.id, e);
                return;
            }
        };

        let result: mlua::Result<Value> = self.process_fn.call::<Value>((table, cfg, pf));
        match &result {
            Err(e) => eprintln!("keyforge[{}]: process error: {}", self.meta.id, e),
            Ok(val) => {
                if val.as_table().is_none() {
                    eprintln!(
                        "keyforge[{}]: process returned non-table: {:?}",
                        self.meta.id, val
                    );
                }
            }
        }
        if let Ok(val) = result
            && let Some(t) = val.as_table()
        {
            apply_result(event, t);
        }
        if let Ok(mut v) = emits_buf.lock() {
            ctx.emits.append(&mut v);
        }
        if let Ok(d) = drop_flag.lock()
            && *d
        {
            ctx.drop_original = true;
        }
    }
}

fn build_event_table(lua: &Lua, event: &Event) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    match event {
        Event::Stick { x, y, side } => {
            t.set("kind", "stick")?;
            t.set("x", *x)?;
            t.set("y", *y)?;
            t.set("side", side.as_str())?;
        }
        Event::Trigger { value, side } => {
            t.set("kind", "trigger")?;
            t.set("value", *value)?;
            t.set("side", side.as_str())?;
        }
        Event::Button { code, pressed } => {
            t.set("kind", "button")?;
            t.set("code", *code)?;
            t.set("pressed", *pressed)?;
        }
    }
    Ok(t)
}

fn build_cfg_table(lua: &Lua, settings: &HashMap<String, String>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (k, v) in settings {
        t.set(k.as_str(), v.as_str())?;
    }
    Ok(t)
}

fn lua_to_i32(t: &Table, key: &str) -> Option<i32> {
    if let Ok(v) = t.get::<i32>(key) {
        return Some(v);
    }
    if let Ok(v) = t.get::<f64>(key) {
        return Some(v as i32);
    }
    None
}
fn apply_result(event: &mut Event, t: &Table) {
    match event {
        Event::Stick { x, y, .. } => {
            if let Some(v) = lua_to_i32(t, "x") {
                *x = v;
            }
            if let Some(v) = lua_to_i32(t, "y") {
                *y = v;
            }
        }
        Event::Trigger { value, .. } => {
            if let Some(v) = lua_to_i32(t, "value") {
                *value = v;
            }
        }
        Event::Button { pressed, .. } => {
            if let Ok(v) = t.get::<bool>("pressed") {
                *pressed = v;
            }
        }
    }
}

pub fn load_plugins(
    lua: &Lua,
    plugin_dir: &str,
    pipeline: &mut Pipeline,
    config_values: &HashMap<String, String>,
    order: &[String],
) -> Vec<PluginMeta> {
    let mut metas = Vec::new();
    let _ = crate::uhid::register_uh(lua);
    let dir = match fs::read_dir(plugin_dir) {
        Ok(d) => d,
        Err(_) => {
            eprintln!("keyforge: plugin dir not found: {}", plugin_dir);
            return metas;
        }
    };
    // Filesystem order is arbitrary; load alphabetically so the baseline
    // chain order is reproducible before applying the user's order.
    let mut entries: Vec<_> = dir.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lua") {
            continue;
        }
        let source = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("keyforge: read {:?}: {}", path, e);
                continue;
            }
        };
        let chunk: Value = match lua.load(&source).eval() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("keyforge: load {:?}: {}", path, e);
                continue;
            }
        };
        let table: &Table = match chunk.as_table() {
            Some(t) => t,
            None => {
                eprintln!("keyforge: {:?} not a table", path);
                continue;
            }
        };
        let id: String = table
            .get::<String>("id")
            .unwrap_or_else(|_| path.file_stem().unwrap().to_string_lossy().to_string());
        let name: String = table.get::<String>("name").unwrap_or_else(|_| id.clone());
        let version: String = table
            .get::<String>("version")
            .unwrap_or_else(|_| String::from("0.1.0"));
        let author: String = table
            .get::<String>("author")
            .unwrap_or_else(|_| String::from("unknown"));
        let description: Option<String> = table.get::<String>("description").ok();
        let enabled: bool = config_values
            .get(&format!("plugin.{}", id))
            .map(|v| v == "1")
            .unwrap_or(true);
        let mut settings = Vec::new();
        if let Ok(arr) = table.get::<Vec<Value>>("settings") {
            for item in arr {
                if let Some(st) = item.as_table() {
                    settings.push(SettingDef {
                        key: st_get(st, "key").unwrap_or_default(),
                        label: st_get(st, "label").unwrap_or_default(),
                        kind: st_get(st, "kind").unwrap_or_else(|| "number".into()),
                        default: st_get(st, "default").unwrap_or_else(|| "0".into()),
                        min: st.get::<i32>("min").ok(),
                        max: st.get::<i32>("max").ok(),
                    });
                }
            }
        }
        let process_fn: Function = match table.get::<Function>("process") {
            Ok(f) => f,
            Err(_) => {
                eprintln!("keyforge: {} missing process()", id);
                continue;
            }
        };
        // call init() if present (for one-time setup like device selection)
        if let Ok(init_fn) = table.get::<Function>("init") {
            let cfg = lua.create_table().ok();
            let pf = api::build_pf(
                lua,
                &std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                &std::sync::Arc::new(std::sync::Mutex::new(false)),
                0,
                0,
            )
            .ok();
            if let (Some(c), Some(p)) = (cfg, pf) {
                for (k, v) in config_values {
                    let _ = c.set(k.as_str(), v.as_str());
                }
                let _: mlua::Result<()> = init_fn.call((c, p));
            }
        }
        // Load per-plugin config
        let mut plugin_config = HashMap::new();
        let conf_path = format!("/sdcard/.keyforge/configs/{}.conf", id);
        if let Ok(conf_raw) = fs::read_to_string(&conf_path) {
            for line in conf_raw.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    plugin_config.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
        let meta = PluginMeta {
            id: id.clone(),
            name,
            version,
            author,
            description,
            enabled,
            settings,
        };
        if meta.enabled {
            eprintln!(
                "keyforge: plugin loaded: {} ({} config keys)",
                id,
                plugin_config.len()
            );
            pipeline.add(Box::new(LuaProcessor {
                meta: meta.clone(),
                lua: lua.clone(),
                process_fn,
                plugin_config,
            }));
        } else {
            eprintln!("keyforge: plugin disabled: {}", id);
        }
        metas.push(meta);
    }
    // Apply the user's chain order (WebUI "plugin_order") on top of the
    // alphabetical baseline so the manifest mirrors the actual pipeline.
    pipeline.reorder(order);
    metas.sort_by_key(|meta| match order.iter().position(|id| id == &meta.id) {
        Some(index) => (index, String::new()),
        None => (order.len(), meta.id.clone()),
    });
    let m = Manifest {
        plugins: metas.clone(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&m)
        && let Err(e) = fs::write(PLUGIN_MANIFEST, &json)
    {
        eprintln!("keyforge: manifest write error: {}", e);
    }
    eprintln!("keyforge: {} plugins loaded", metas.len());
    metas
}

fn st_get(table: &Table, key: &str) -> Option<String> {
    table.get::<String>(key).ok().or_else(|| {
        table.get::<Value>(key).ok().and_then(|v| match v {
            Value::String(s) => Some(s.to_str().ok()?.to_string()),
            Value::Integer(n) => Some(n.to_string()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Side;

    const DEADZONE_PLUGIN: &str = r#"
return {
  id = "deadzone", name = "Deadzone Filter", version = "1.1.0", author = "keyforge",
  settings = {
    { key = "deadzone_left", label = "Left", kind = "permille", default = "91", min = 0, max = 1000 },
  },
  process = function(ev, cfg, pf)
    if ev.kind ~= "stick" then return ev end
    local dz = tonumber(cfg["deadzone_" .. ev.side]) or 0
    local thr = 32767 * dz / 1000
    local dist = math.sqrt(ev.x * ev.x + ev.y * ev.y)
    if dist <= thr then
      pf.drop()
      pf.emit(pf.EV_ABS, 0, 0)
      pf.emit(pf.EV_ABS, 1, 0)
      return {}
    end
    return ev
  end
}"#;

    #[test]
    fn deadzone_plugin_transforms_stick_events() {
        let dir = std::env::temp_dir().join(format!("keyforge-plugins-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("deadzone.lua"), DEADZONE_PLUGIN).unwrap();

        let lua = Lua::new();
        let mut settings = HashMap::new();
        settings.insert("deadzone_left".to_string(), "500".to_string());
        let mut pipeline = Pipeline::new();
        let metas = load_plugins(
            &lua,
            dir.to_str().unwrap(),
            &mut pipeline,
            &HashMap::new(),
            &[],
        );
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, "deadzone");
        assert_eq!(pipeline.plugin_ids(), vec!["deadzone".to_string()]);
        // pf.drop() + pf.emit() replace the event content (folded), so the
        // event is kept and carries the plugin's zeroed axes.
        let mut event = Event::Stick {
            x: 100,
            y: 40,
            side: Side::Left,
        };
        let (emits, dropped) = pipeline.run(&mut event, &settings);
        assert!(!dropped);
        assert!(emits.is_empty());
        assert_eq!((event.x(), event.y()), (0, 0));

        let mut outside = Event::Stick {
            x: 30000,
            y: 0,
            side: Side::Left,
        };
        let (emits, dropped) = pipeline.run(&mut outside, &settings);
        assert!(!dropped);
        assert_eq!(emits.len(), 0);
        assert_eq!(outside.x(), 30000);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn emitted_replacements_reach_later_plugins() {
        let dir = std::env::temp_dir().join(format!("keyforge-chain-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("replace.lua"),
            r#"
return {
  id = "replace",
  process = function(ev, cfg, pf)
    if ev.kind ~= "stick" then return ev end
    pf.drop()
    pf.emit(pf.EV_ABS, 0, 5)
    pf.emit(pf.EV_ABS, 1, 6)
    return {}
  end
}"#,
        )
        .unwrap();
        fs::write(
            dir.join("scale.lua"),
            r#"
return {
  id = "scale",
  process = function(ev, cfg, pf)
    if ev.kind ~= "stick" then return ev end
    ev.x = ev.x * 2
    ev.y = ev.y * 2
    return ev
  end
}"#,
        )
        .unwrap();

        let lua = Lua::new();
        let mut settings = HashMap::new();
        settings.insert("plugin.scale".to_string(), "1".to_string());

        // replace → scale: scale must see the replaced values.
        let mut pipeline = Pipeline::new();
        let metas = load_plugins(&lua, dir.to_str().unwrap(), &mut pipeline, &settings, &[]);
        pipeline.sort_steps_by_id();
        assert_eq!(
            pipeline.plugin_ids(),
            vec!["replace".to_string(), "scale".to_string()]
        );
        assert_eq!(metas.len(), 2);
        let mut event = Event::Stick {
            x: 100,
            y: 40,
            side: Side::Left,
        };
        let (emits, dropped) = pipeline.run(&mut event, &settings);
        assert!(!dropped);
        assert!(emits.is_empty());
        assert_eq!((event.x(), event.y()), (10, 12));

        // scale → replace: replacement wins.
        let mut reversed = Pipeline::new();
        let _ = load_plugins(&lua, dir.to_str().unwrap(), &mut reversed, &settings, &[]);
        reversed.sort_steps_by_id();
        reversed.reverse_steps();
        let (emits, dropped) = reversed.run(&mut event, &settings);
        assert!(!dropped);
        assert!(emits.is_empty());
        assert_eq!((event.x(), event.y()), (5, 6));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn button_remaps_fold_into_the_event() {
        let dir = std::env::temp_dir().join(format!("keyforge-remap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("remap.lua"),
            r#"
return {
  id = "remap",
  process = function(ev, cfg, pf)
    if ev.kind ~= "button" or ev.code ~= 304 then return ev end
    pf.emit(pf.EV_KEY, 311, ev.pressed and 1 or 0)
    pf.drop()
    return ev
  end
}"#,
        )
        .unwrap();

        let lua = Lua::new();
        let mut pipeline = Pipeline::new();
        let _ = load_plugins(
            &lua,
            dir.to_str().unwrap(),
            &mut pipeline,
            &HashMap::new(),
            &[],
        );
        let mut event = Event::Button {
            code: 304,
            pressed: true,
        };
        let (emits, dropped) = pipeline.run(&mut event, &HashMap::new());
        assert!(!dropped);
        assert!(emits.is_empty());
        assert_eq!((event.code(), event.pressed()), (311, true));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_order_follows_the_requested_sequence() {
        let dir = std::env::temp_dir().join(format!("keyforge-order-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for id in ["deadzone", "square"] {
            fs::write(
                dir.join(format!("{id}.lua")),
                format!(
                    r#"return {{ id = "{id}", process = function(ev, cfg, pf) return ev end }}"#
                ),
            )
            .unwrap();
        }

        let lua = Lua::new();
        let mut pipeline = Pipeline::new();
        let metas = load_plugins(
            &lua,
            dir.to_str().unwrap(),
            &mut pipeline,
            &HashMap::new(),
            &["square".to_string(), "deadzone".to_string()],
        );
        let order_ids: Vec<&str> = metas.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(order_ids, ["square", "deadzone"]);
        assert_eq!(
            pipeline.plugin_ids(),
            ["square".to_string(), "deadzone".to_string()]
        );

        // Unlisted plugins fall back to the alphabetical baseline after the
        // listed ones.
        let mut baseline = Pipeline::new();
        let baseline_metas = load_plugins(
            &lua,
            dir.to_str().unwrap(),
            &mut baseline,
            &HashMap::new(),
            &[],
        );
        let baseline_ids: Vec<&str> = baseline_metas.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(baseline_ids, ["deadzone", "square"]);
        assert_eq!(
            baseline.plugin_ids(),
            ["deadzone".to_string(), "square".to_string()]
        );

        fs::remove_dir_all(dir).unwrap();
    }
}
