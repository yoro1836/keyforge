use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub struct Config {
    pub vid: u16,
    pub pid: u16,
    pub plugin_dir: String,
    pub hide_device: bool,
    pub plugin_order: Vec<String>,
    pub values: HashMap<String, String>,
}

impl Config {
    pub fn load(path: &Path) -> Self {
        let mut cfg = Config {
            vid: 0x045e,
            pid: 0x028e,
            plugin_dir: "/sdcard/.keyforge/plugins".into(),
            hide_device: false,
            plugin_order: Vec::new(),
            values: HashMap::new(),
        };
        if let Ok(file) = fs::File::open(path) {
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = trimmed.split_once('=') {
                    let key = k.trim().to_lowercase();
                    let val = v.trim().to_string();
                    match key.as_str() {
                        "vid" => cfg.vid = parse_hex16(&val),
                        "pid" => cfg.pid = parse_hex16(&val),
                        "plugin_dir" => cfg.plugin_dir = val,
                        "hide_device" => cfg.hide_device = parse_bool(&val),
                        "plugin_order" => {
                            cfg.plugin_order = val
                                .split(',')
                                .map(|item| item.trim().to_string())
                                .filter(|item| !item.is_empty())
                                .collect();
                        }
                        _ => {
                            cfg.values.insert(key, val);
                        }
                    }
                }
            }
        }
        cfg
    }
}

impl Config {
    /// Rewrite VID/PID lines in place (atomic tmp + rename), preserving
    /// comments, key case, and every other line. Creates the file when absent
    /// so Lua-driven source switches survive restarts and stay in sync with
    /// the WebUI.
    pub fn persist_source(path: &Path, vid: u16, pid: u16) -> std::io::Result<()> {
        let mut lines: Vec<String> = Vec::new();
        if path.exists() {
            let raw = fs::read_to_string(path)?;
            lines = raw.lines().map(str::to_string).collect();
        }
        let mut saw_vid = false;
        let mut saw_pid = false;
        for line in lines.iter_mut() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some((key, _)) = trimmed.split_once('=') {
                match key.trim().to_lowercase().as_str() {
                    "vid" => {
                        *line = format!("{}=0x{:04x}", key.trim(), vid);
                        saw_vid = true;
                    }
                    "pid" => {
                        *line = format!("{}=0x{:04x}", key.trim(), pid);
                        saw_pid = true;
                    }
                    _ => {}
                }
            }
        }
        if !saw_vid {
            lines.push(format!("VID=0x{vid:04x}"));
        }
        if !saw_pid {
            lines.push(format!("PID=0x{pid:04x}"));
        }
        let tmp = path.with_extension("tmp");

        fs::write(&tmp, lines.join("\n") + "\n")?;
        fs::rename(tmp, path)?;
        Ok(())
    }
}

fn parse_hex16(s: &str) -> u16 {
    let s = s.trim().strip_prefix("0x").unwrap_or(s);
    u16::from_str_radix(s, 16).unwrap_or(0)
}

fn parse_bool(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_hiding_without_exposing_it_to_plugins() {
        let path =
            std::env::temp_dir().join(format!("keyforge-config-{}.conf", std::process::id()));
        fs::write(
            &path,
            "VID=0x054c\nPID=0x0ce6\nHIDE_DEVICE=on\nplugin.deadzone=1\nPLUGIN_ORDER=square,deadzone\n",
        )
        .unwrap();

        let config = Config::load(&path);
        assert!(config.hide_device);
        assert_eq!(config.vid, 0x054c);
        assert_eq!(config.pid, 0x0ce6);
        assert_eq!(
            config.values.get("plugin.deadzone").map(String::as_str),
            Some("1")
        );
        assert_eq!(config.plugin_order, vec!["square", "deadzone"]);
        assert!(!config.values.contains_key("plugin_order"));
        assert!(!config.values.contains_key("hide_device"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn persist_source_rewrites_ids_preserving_layout() {
        let path =
            std::env::temp_dir().join(format!("keyforge-source-{}.conf", std::process::id()));
        fs::write(&path, "# comment\nVID=0x045e\nPID=0x028e\nPLUGIN_DIR=/x\n").unwrap();

        Config::persist_source(&path, 0x054c, 0x0ce6).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# comment"));
        assert!(raw.contains("VID=0x054c"));
        assert!(raw.contains("PID=0x0ce6"));
        assert!(raw.contains("PLUGIN_DIR=/x"));
        let config = Config::load(&path);
        assert_eq!((config.vid, config.pid), (0x054c, 0x0ce6));

        fs::remove_file(&path).unwrap();
        Config::persist_source(&path, 0x1234, 0x5678).unwrap();
        let config = Config::load(&path);
        assert_eq!((config.vid, config.pid), (0x1234, 0x5678));
        fs::remove_file(&path).unwrap();
    }
}
