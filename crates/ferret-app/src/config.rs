//! `[app]` table in ~/.config/ferret/config.toml, hotkey strings, Full Disk Access probe, login item.
use std::io;
use std::path::PathBuf;
use std::str::FromStr;

use global_hotkey::hotkey::{HotKey, Modifiers};

pub const DEFAULT_HOTKEY: &str = "Option+Space";

#[derive(Clone, Debug, PartialEq)]
pub struct AppConfig {
    pub hotkey: String,
    /// Globs ferretd and `ff index` skip, on top of the built-in excludes.
    pub excluded: Vec<String>,
    pub external_drives: bool,
    pub launch_at_login: bool,
    /// `[mcp] contents`: let agents use search_contents.
    pub mcp_contents: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: DEFAULT_HOTKEY.into(),
            excluded: Vec::new(),
            external_drives: false,
            launch_at_login: false,
            mcp_contents: false,
        }
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
    let mcp_contents = table
        .get("mcp")
        .and_then(|m| m.get("contents"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let Some(app) = table.get("app").and_then(|v| v.as_table()) else {
        return AppConfig { mcp_contents, ..d };
    };
    AppConfig {
        mcp_contents,
        hotkey: app
            .get("hotkey")
            .and_then(|v| v.as_str())
            .map_or(d.hotkey, String::from),
        excluded: app
            .get("excluded")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        external_drives: app
            .get("external_drives")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        launch_at_login: app
            .get("launch_at_login")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

/// Replace only the [app] keys and `[mcp] contents`; every other key, comment and layout is kept.
pub fn write_app(text: &str, cfg: &AppConfig) -> Result<String, toml_edit::TomlError> {
    use toml_edit::{value, Array, DocumentMut, Item, Table};
    let mut doc = text.parse::<DocumentMut>()?;
    if !doc.contains_table("app") {
        doc["app"] = Item::Table(Table::new());
    }
    let app = &mut doc["app"];
    app["hotkey"] = value(cfg.hotkey.as_str());
    app["excluded"] = value(cfg.excluded.iter().map(String::as_str).collect::<Array>());
    app["external_drives"] = value(cfg.external_drives);
    app["launch_at_login"] = value(cfg.launch_at_login);
    if !doc.contains_table("mcp") {
        doc["mcp"] = Item::Table(Table::new());
    }
    doc["mcp"]["contents"] = value(cfg.mcp_contents);
    Ok(doc.to_string())
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
    // Temp file + rename: a crash mid-write never leaves a truncated config.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, new)?;
    std::fs::rename(tmp, path)
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
    out.push_str(
        key.strip_prefix("Key")
            .or_else(|| key.strip_prefix("Digit"))
            .unwrap_or(&key),
    );
    out
}

/// "Option+Space" -> "⌥Space" for menus and the settings field.
pub fn hotkey_glyphs(s: &str) -> String {
    s.replace("Control+", "⌃")
        .replace("Option+", "⌥")
        .replace("Shift+", "⇧")
        .replace("Command+", "⌘")
}

/// Hotkey string from what egui saw while recording. None for bare keys (no modifier).
pub fn hotkey_from_egui(m: eframe::egui::Modifiers, key: eframe::egui::Key) -> Option<String> {
    if !(m.ctrl || m.alt || m.mac_cmd) {
        return None;
    }
    let mut s = String::new();
    for (on, name) in [
        (m.ctrl, "Control+"),
        (m.alt, "Option+"),
        (m.shift, "Shift+"),
        (m.mac_cmd, "Command+"),
    ] {
        if on {
            s.push_str(name);
        }
    }
    s.push_str(key.name());
    parse_hotkey(&s).map(|h| format_hotkey(&h))
}

/// True = Full Disk Access is clearly missing. Only PermissionDenied counts; NotFound etc. tell us nothing.
pub fn fda_missing(probe: &[io::Result<std::fs::ReadDir>]) -> bool {
    probe
        .iter()
        .any(|r| matches!(r, Err(e) if e.kind() == io::ErrorKind::PermissionDenied))
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
    fda_missing(&[
        std::fs::read_dir(h.join("Library/Mail")),
        std::fs::read_dir(h.join("Library/Safari")),
    ])
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
    let exe = exe
        .to_string_lossy()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
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

/// `ff` shipped next to this binary in Ferret.app/Contents/MacOS (also true for cargo builds).
pub fn bundled_ff() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.canonicalize().ok()?.with_file_name("ff");
    p.is_file().then_some(p)
}

/// Command line for `claude mcp add`; the absolute path works without `ff` on PATH.
pub fn mcp_line() -> String {
    let ff = bundled_ff().map_or("ff".into(), |p| p.to_string_lossy().into_owned());
    let ff = if ff.contains(' ') { format!("'{ff}'") } else { ff };
    format!("claude mcp add --scope user ferret -- {ff} mcp")
}

/// Symlink /usr/local/bin/ff to the bundled ff, asking for an admin password.
pub fn install_cli() -> Result<String, String> {
    let ff = bundled_ff().ok_or("ff is not inside this app")?;
    let ff = ff.to_string_lossy();
    if ff.contains(['\'', '"', '\\']) {
        return Err(format!("Unsupported characters in {ff}"));
    }
    let script = format!(
        "do shell script \"mkdir -p /usr/local/bin && ln -sf '{ff}' /usr/local/bin/ff\" with administrator privileges"
    );
    let out = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok("Installed: run `ff` in any new terminal.".into())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// `ff daemon install`: the launchd agent that keeps the index live.
pub fn start_daemon() -> Result<String, String> {
    let ff = bundled_ff().ok_or("ff is not inside this app")?;
    let out = std::process::Command::new(ff)
        .args(["daemon", "install"])
        .output()
        .map_err(|e| e.to_string())?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    if out.status.success() {
        Ok(text(&out.stdout))
    } else {
        Err(text(&out.stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use global_hotkey::hotkey::Code;

    #[test]
    fn app_table_round_trip_keeps_mcp() {
        let old = "# my notes\n[mcp]\ndeny = [\"~/.ssh\"] # keep keys out\n\n[app]\nhotkey = \"Command+K\"\n";
        assert_eq!(read_app(old).hotkey, "Command+K");
        let cfg = AppConfig {
            excluded: vec!["~/Library/Caches".into()],
            launch_at_login: true,
            mcp_contents: true,
            ..Default::default()
        };
        let new = write_app(old, &cfg).unwrap();
        let t: toml::Table = new.parse().unwrap();
        assert_eq!(t["mcp"]["deny"][0].as_str(), Some("~/.ssh"));
        assert_eq!(t["mcp"]["contents"].as_bool(), Some(true));
        assert!(read_app("[mcp]\ncontents = true").mcp_contents);
        assert!(
            new.contains("# my notes") && new.contains("# keep keys out"),
            "{new}"
        );
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
        assert_eq!(
            hotkey_from_egui(
                M {
                    alt: true,
                    ..M::NONE
                },
                Key::Space
            )
            .as_deref(),
            Some("Option+Space")
        );
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
