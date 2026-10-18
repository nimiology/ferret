//! `[app]` table in ~/.config/ferret/config.toml, hotkey strings, Full Disk Access probe, login item.
use std::io;
use std::path::PathBuf;
use std::str::FromStr;

use global_hotkey::hotkey::{HotKey, Modifiers};

pub const DEFAULT_HOTKEY: &str = "Option+Space";

#[derive(Clone, Debug, PartialEq)]
pub struct AppConfig {
    pub hotkey: String,
    /// Stored for the daemon; ferretd does not read it yet.
    pub excluded: Vec<String>,
    pub external_drives: bool,
    pub launch_at_login: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self { hotkey: DEFAULT_HOTKEY.into(), excluded: Vec::new(), external_drives: false, launch_at_login: false }
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into()))
}

pub fn config_path() -> PathBuf {
    home().join(".config/ferret/config.toml")
}

/// Missing file / missing [app] / bad values -> defaults for those fields.
pub fn read_app(text: &str) -> AppConfig {
    let d = AppConfig::default();
    let table = text.parse::<toml::Table>().unwrap_or_default();
    let Some(app) = table.get("app").and_then(|v| v.as_table()) else { return d };
    AppConfig {
        hotkey: app.get("hotkey").and_then(|v| v.as_str()).map_or(d.hotkey, String::from),
        excluded: app
            .get("excluded")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        external_drives: app.get("external_drives").and_then(|v| v.as_bool()).unwrap_or(false),
        launch_at_login: app.get("launch_at_login").and_then(|v| v.as_bool()).unwrap_or(false),
    }
}

/// Replace only the [app] table; every other table ([mcp], …) is kept.
pub fn write_app(text: &str, cfg: &AppConfig) -> Result<String, toml::de::Error> {
    let mut table = text.parse::<toml::Table>()?;
    let mut app = toml::Table::new();
    app.insert("hotkey".into(), cfg.hotkey.clone().into());
    app.insert("excluded".into(), toml::Value::Array(cfg.excluded.iter().map(|s| s.clone().into()).collect()));
    app.insert("external_drives".into(), cfg.external_drives.into());
    app.insert("launch_at_login".into(), cfg.launch_at_login.into());
    table.insert("app".into(), app.into());
    Ok(toml::to_string(&table).expect("table serializes"))
}

pub fn load() -> AppConfig {
    read_app(&std::fs::read_to_string(config_path()).unwrap_or_default())
}

pub fn save(cfg: &AppConfig) -> io::Result<()> {
    let path = config_path();
    let old = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    // Refuse to overwrite a file we can't parse: that would drop the user's other tables.
    let new = write_app(&old, cfg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, new)
}

/// "Option+Space" -> HotKey. Accepts what `format_hotkey` writes plus global-hotkey's own names.
pub fn parse_hotkey(s: &str) -> Option<HotKey> {
    HotKey::from_str(s).ok()
}

/// HotKey -> "Control+Option+Shift+Command+K" (mac modifier order).
pub fn format_hotkey(h: &HotKey) -> String {
    let mut out = String::new();
    for (m, name) in [
        (Modifiers::CONTROL, "Control+"),
        (Modifiers::ALT, "Option+"),
        (Modifiers::SHIFT, "Shift+"),
        (Modifiers::META, "Command+"),
    ] {
        if h.mods.contains(m) {
            out.push_str(name);
        }
    }
    let key = h.key.to_string();
    out.push_str(key.strip_prefix("Key").or_else(|| key.strip_prefix("Digit")).unwrap_or(&key));
    out
}

/// "Option+Space" -> "⌥Space" for menus and the settings field.
pub fn hotkey_glyphs(s: &str) -> String {
    s.replace("Control+", "⌃").replace("Option+", "⌥").replace("Shift+", "⇧").replace("Command+", "⌘")
}

/// Hotkey string from what egui saw while recording. None for bare keys (no modifier).
pub fn hotkey_from_egui(m: eframe::egui::Modifiers, key: eframe::egui::Key) -> Option<String> {
    if !(m.ctrl || m.alt || m.mac_cmd) {
        return None;
    }
    let mut s = String::new();
    for (on, name) in [(m.ctrl, "Control+"), (m.alt, "Option+"), (m.shift, "Shift+"), (m.mac_cmd, "Command+")] {
        if on {
            s.push_str(name);
        }
    }
    s.push_str(key.name());
    parse_hotkey(&s).map(|h| format_hotkey(&h))
}

/// True = Full Disk Access is clearly missing. Only PermissionDenied counts; NotFound etc. tell us nothing.
pub fn fda_missing(probe: &[io::Result<std::fs::ReadDir>]) -> bool {
    probe.iter().any(|r| matches!(r, Err(e) if e.kind() == io::ErrorKind::PermissionDenied))
}

fn fda_marker() -> PathBuf {
    home().join("Library/Application Support/ferret/fda-prompted")
}

/// Show screen 05? Once, and only if FDA looks missing.
pub fn should_prompt_fda() -> bool {
    if fda_marker().exists() {
        return false;
    }
    let h = home();
    fda_missing(&[std::fs::read_dir(h.join("Library/Mail")), std::fs::read_dir(h.join("Library/Safari"))])
}

pub fn dismiss_fda() {
    let m = fda_marker();
    let _ = std::fs::create_dir_all(m.parent().unwrap());
    let _ = std::fs::write(m, b"");
}

fn launch_agent() -> PathBuf {
    home().join("Library/LaunchAgents/com.ferret.app.plist")
}

pub fn set_launch_at_login(on: bool) -> io::Result<()> {
    let path = launch_agent();
    if !on {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        };
    }
    let exe = std::env::current_exe()?;
    let exe = exe.to_string_lossy().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.ferret.app</string>
  <key>ProgramArguments</key><array><string>{exe}</string></array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#
    );
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, plist)
}

#[cfg(test)]
mod tests {
    use super::*;
    use global_hotkey::hotkey::Code;

    #[test]
    fn app_table_round_trip_keeps_mcp() {
        let old = "[mcp]\ndeny = [\"~/.ssh\"]\n\n[app]\nhotkey = \"Command+K\"\n";
        assert_eq!(read_app(old).hotkey, "Command+K");
        let cfg = AppConfig { excluded: vec!["~/Library/Caches".into()], launch_at_login: true, ..Default::default() };
        let new = write_app(old, &cfg).unwrap();
        let t: toml::Table = new.parse().unwrap();
        assert_eq!(t["mcp"]["deny"][0].as_str(), Some("~/.ssh"));
        assert_eq!(read_app(&new), cfg);
        assert_eq!(read_app(""), AppConfig::default());
        assert!(write_app("not = [toml", &cfg).is_err());
    }

    #[test]
    fn hotkey_strings() {
        let h = parse_hotkey("Option+Space").unwrap();
        assert_eq!(h, HotKey::new(Some(Modifiers::ALT), Code::Space));
        assert_eq!(format_hotkey(&h), "Option+Space");
        let k = HotKey::new(Some(Modifiers::META | Modifiers::SHIFT), Code::KeyK);
        assert_eq!(format_hotkey(&k), "Shift+Command+K");
        assert_eq!(parse_hotkey(&format_hotkey(&k)), Some(k));
        assert_eq!(hotkey_glyphs("Option+Space"), "⌥Space");
        assert!(parse_hotkey("Option+Nope").is_none());
        use eframe::egui::{Key, Modifiers as M};
        assert_eq!(hotkey_from_egui(M { alt: true, ..M::NONE }, Key::Space).as_deref(), Some("Option+Space"));
        assert_eq!(hotkey_from_egui(M::NONE, Key::A), None);
    }

    #[test]
    fn fda_probe() {
        let denied = || Err(io::Error::from(io::ErrorKind::PermissionDenied));
        let missing = || Err(io::Error::from(io::ErrorKind::NotFound));
        assert!(fda_missing(&[missing(), denied()]));
        assert!(!fda_missing(&[missing(), missing()]));
        assert!(!fda_missing(&[std::fs::read_dir("/")]));
    }
}
