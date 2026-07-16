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
    pub id: String, pub name: String, pub version: String, pub author: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool, pub settings: Vec<SettingDef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingDef {
    pub key: String, pub label: String, pub kind: String, pub default: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub min: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")] pub max: Option<i32>,
}

#[derive(Serialize)] pub struct Manifest { pub plugins: Vec<PluginMeta> }

pub struct LuaProcessor {
    #[allow(dead_code)]
    pub meta: PluginMeta,
    lua: &'static Lua,
    process_fn: Function,
    /// Per-plugin config loaded from /sdcard/.keyforge/configs/<id>.conf
    plugin_config: HashMap<String, String>,
}

impl Processor for LuaProcessor {
    fn id(&self) -> &str { &self.meta.id }

    fn process(&self, event: &mut Event, ctx: &mut Ctx) {
        let table = match build_event_table(self.lua, event) { Ok(t) => t, Err(e) => { eprintln!("keyforge[{}]: build_event_table: {}", self.meta.id, e); return; } };
        // Merge global config with per-plugin config (per-plugin takes priority)
        let mut merged = ctx.settings.clone();
        for (k, v) in &self.plugin_config { merged.insert(k.clone(), v.clone()); }
        let cfg = match build_cfg_table(self.lua, &merged) { Ok(t) => t, Err(e) => { eprintln!("keyforge[{}]: build_cfg_table: {}", self.meta.id, e); return; } };

        let emits_buf: Arc<Mutex<Vec<EmitEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let drop_flag: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let raw_x = match event { Event::Stick { x, .. } => *x, _ => 0 };
        let raw_y = match event { Event::Stick { y, .. } => *y, _ => 0 };
        let pf = match api::build_pf(self.lua, &emits_buf, &drop_flag, raw_x, raw_y) { Ok(t) => t, Err(e) => { eprintln!("keyforge[{}]: build_pf: {}", self.meta.id, e); return; } };

        let result: mlua::Result<Value> = self.process_fn.call::<Value>((table, cfg, pf));
        match &result {
            Err(e) => eprintln!("keyforge[{}]: process error: {}", self.meta.id, e),
            Ok(val) => if val.as_table().is_none() { eprintln!("keyforge[{}]: process returned non-table: {:?}", self.meta.id, val); }
        }
        if let Ok(val) = result
            && let Some(t) = val.as_table()
        {
            apply_result(event, t);
        }
        if let Ok(mut v) = emits_buf.lock() { ctx.emits.append(&mut v); }
        if let Ok(d) = drop_flag.lock()
            && *d { ctx.drop_original = true; }
    }
}

fn build_event_table(lua: &Lua, event: &Event) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    match event {
        Event::Stick { x, y, side } => { t.set("kind", "stick")?; t.set("x", *x)?; t.set("y", *y)?; t.set("side", side.as_str())?; }
        Event::Trigger { value, side } => { t.set("kind", "trigger")?; t.set("value", *value)?; t.set("side", side.as_str())?; }
        Event::Button { code, pressed } => { t.set("kind", "button")?; t.set("code", *code)?; t.set("pressed", *pressed)?; }
    }
    Ok(t)
}

fn build_cfg_table(lua: &Lua, settings: &HashMap<String, String>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (k, v) in settings { t.set(k.as_str(), v.as_str())?; }
    Ok(t)
}

fn lua_to_i32(t: &Table, key: &str) -> Option<i32> {
    if let Ok(v) = t.get::<i32>(key) { return Some(v); }
    if let Ok(v) = t.get::<f64>(key) { return Some(v as i32); }
    None
}
fn apply_result(event: &mut Event, t: &Table) {
    match event {
        Event::Stick { x, y, .. } => {
            if let Some(v) = lua_to_i32(t, "x") { *x = v; }
            if let Some(v) = lua_to_i32(t, "y") { *y = v; }
        }
        Event::Trigger { value, .. } => { if let Some(v) = lua_to_i32(t, "value") { *value = v; } }
        Event::Button { pressed, .. } => { if let Ok(v) = t.get::<bool>("pressed") { *pressed = v; } }
    }
}

pub fn load_plugins(
    lua: &'static Lua, plugin_dir: &str, pipeline: &mut Pipeline,
    config_values: &HashMap<String, String>,
) -> Vec<PluginMeta> {
    let mut metas = Vec::new();
    let dir = match fs::read_dir(plugin_dir) { Ok(d) => d, Err(_) => { eprintln!("keyforge: plugin dir not found: {}", plugin_dir); return metas; } };
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lua") { continue; }
        let source = match fs::read_to_string(&path) { Ok(s) => s, Err(e) => { eprintln!("keyforge: read {:?}: {}", path, e); continue; } };
        let chunk: Value = match lua.load(&source).eval() { Ok(v) => v, Err(e) => { eprintln!("keyforge: load {:?}: {}", path, e); continue; } };
        let table: &Table = match chunk.as_table() { Some(t) => t, None => { eprintln!("keyforge: {:?} not a table", path); continue; } };
        let id: String = table.get::<String>("id").unwrap_or_else(|_| path.file_stem().unwrap().to_string_lossy().to_string());
        let name: String = table.get::<String>("name").unwrap_or_else(|_| id.clone());
        let version: String = table.get::<String>("version").unwrap_or_else(|_| String::from("0.1.0"));
        let author: String = table.get::<String>("author").unwrap_or_else(|_| String::from("unknown"));
        let description: Option<String> = table.get::<String>("description").ok();
        let enabled: bool = config_values.get(&format!("plugin.{}", id)).map(|v| v == "1").unwrap_or(true);
        let mut settings = Vec::new();
        if let Ok(arr) = table.get::<Vec<Value>>("settings") {
            for item in arr { if let Some(st) = item.as_table() {
                settings.push(SettingDef { key: st_get(st, "key").unwrap_or_default(), label: st_get(st, "label").unwrap_or_default(), kind: st_get(st, "kind").unwrap_or_else(|| "number".into()), default: st_get(st, "default").unwrap_or_else(|| "0".into()), min: st.get::<i32>("min").ok(), max: st.get::<i32>("max").ok() });
            }}
        }
        let process_fn: Function = match table.get::<Function>("process") { Ok(f) => f, Err(_) => { eprintln!("keyforge: {} missing process()", id); continue; } };
        // call init() if present (for one-time setup like device selection)
        if let Ok(init_fn) = table.get::<Function>("init") {
            let cfg = lua.create_table().ok();
            let pf = api::build_pf(lua, &std::sync::Arc::new(std::sync::Mutex::new(Vec::new())), &std::sync::Arc::new(std::sync::Mutex::new(false)), 0, 0).ok();
            if let (Some(c), Some(p)) = (cfg, pf) {
                for (k, v) in config_values { let _ = c.set(k.as_str(), v.as_str()); }
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
        let meta = PluginMeta { id: id.clone(), name, version, author, description, enabled, settings };
        if meta.enabled {
            eprintln!("keyforge: plugin loaded: {} ({} config keys)", id, plugin_config.len());
            pipeline.add(Box::new(LuaProcessor { meta: meta.clone(), lua, process_fn, plugin_config }));
        } else {
            eprintln!("keyforge: plugin disabled: {}", id);
        }
        metas.push(meta);
    }
    let m = Manifest { plugins: metas.clone() };
    if let Ok(json) = serde_json::to_string_pretty(&m)
        && let Err(e) = fs::write(PLUGIN_MANIFEST, &json) { eprintln!("keyforge: manifest write error: {}", e); }
    eprintln!("keyforge: {} plugins loaded", metas.len());
    metas
}

fn st_get(table: &Table, key: &str) -> Option<String> {
    table.get::<String>(key).ok().or_else(|| table.get::<Value>(key).ok().and_then(|v| match v {
        Value::String(s) => Some(s.to_str().ok()?.to_string()), Value::Integer(n) => Some(n.to_string()), Value::Number(n) => Some(n.to_string()), _ => None,
    }))
}
