//! `ff daemon install|uninstall|status`: the ferretd launchd agent.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use ferret_core::protocol::{self, Op, Status};

pub const LABEL: &str = "com.ferret.daemon";

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/tmp".into())
}

pub fn plist_path() -> PathBuf {
    home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn log_path() -> PathBuf {
    home().join("Library/Logs/ferret/ferretd.log")
}

/// The `ferretd` binary installed next to `ff`.
pub fn ferretd_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate the ff binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let p = exe.with_file_name("ferretd");
    if !p.is_file() {
        bail!("ferretd not found at {} — install it next to ff (e.g. `cargo build -p ferretd`)", p.display());
    }
    Ok(p)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn plist(ferretd: &Path, log: &Path) -> String {
    let bin = xml_escape(&ferretd.display().to_string());
    let log = xml_escape(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{bin}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>LowPriorityIO</key><true/>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    )
}

fn uid() -> String {
    Command::new("id").arg("-u").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

fn launchctl(args: &[&str]) -> bool {
    Command::new("launchctl").args(args).output().is_ok_and(|o| o.status.success())
}

/// (running, pid) from `launchctl print`; None = not loaded.
pub fn launchd_state() -> Option<(bool, Option<String>)> {
    let o = Command::new("launchctl").arg("print").arg(format!("gui/{}/{LABEL}", uid())).output().ok()?;
    if !o.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&o.stdout);
    let field = |k: &str| text.lines().find_map(|l| l.trim().strip_prefix(k).map(|v| v.trim().to_string()));
    Some((field("state =").as_deref() == Some("running"), field("pid =")))
}

pub fn socket_status() -> Option<Status> {
    protocol::request(Op::Status).ok()?.status
}

pub fn run(action: &str) -> Result<()> {
    match action {
        "install" => install(),
        "uninstall" => uninstall(),
        _ => status(),
    }
}

fn install() -> Result<()> {
    let bin = ferretd_path()?;
    let (p, log) = (plist_path(), log_path());
    for d in [p.parent().unwrap(), log.parent().unwrap()] {
        std::fs::create_dir_all(d).with_context(|| format!("could not create {}", d.display()))?;
    }
    std::fs::write(&p, plist(&bin, &log)).with_context(|| format!("could not write {}", p.display()))?;
    let ps = p.display().to_string();
    let domain = format!("gui/{}", uid());
    launchctl(&["bootout", &format!("{domain}/{LABEL}")]); // reinstall: drop the old one first
    if !launchctl(&["bootstrap", &domain, &ps]) && !launchctl(&["load", "-w", &ps]) {
        bail!("launchctl could not load {ps} (see `launchctl print {domain}/{LABEL}`)");
    }
    println!("installed {LABEL} → {}\n  plist {ps}\n  log   {}", bin.display(), log.display());
    Ok(())
}

fn uninstall() -> Result<()> {
    let p = plist_path();
    let ps = p.display().to_string();
    if !launchctl(&["bootout", &format!("gui/{}/{LABEL}", uid())]) && p.exists() {
        launchctl(&["unload", "-w", &ps]);
    }
    match std::fs::remove_file(&p) {
        Ok(()) => println!("uninstalled {LABEL} (removed {ps})"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("{LABEL} was not installed"),
        Err(e) => return Err(e).with_context(|| format!("could not remove {ps}")),
    }
    Ok(())
}

fn status() -> Result<()> {
    let p = plist_path();
    println!("plist     {} ({})", if p.exists() { "present" } else { "missing" }, p.display());
    match launchd_state() {
        Some((running, pid)) => println!(
            "launchd   loaded, {}{}",
            if running { "running" } else { "not running" },
            pid.map(|p| format!(", pid {p}")).unwrap_or_default()
        ),
        None => println!("launchd   not loaded"),
    }
    match socket_status() {
        Some(s) => {
            println!("socket    reachable ({})", protocol::socket_path().display());
            println!("  files              {}", s.files);
            println!("  indexing           {}", s.indexing);
            println!("  progress           {:.0}%", s.progress * 100.0);
            if let Some(c) = s.current_path {
                println!("  current path       {c}");
            }
            println!("  last event id      {}", s.last_event_id);
            println!("  permission denied  {}", s.permission_denied);
        }
        None => println!("socket    unreachable ({})", protocol::socket_path().display()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_has_required_keys() {
        let s = plist(Path::new("/opt/a&b/ferretd"), Path::new("/Users/x/Library/Logs/ferret/ferretd.log"));
        assert!(s.contains("<key>Label</key><string>com.ferret.daemon</string>"));
        assert!(s.contains("<array><string>/opt/a&amp;b/ferretd</string></array>"));
        for k in ["RunAtLoad", "KeepAlive", "LowPriorityIO"] {
            assert!(s.contains(&format!("<key>{k}</key><true/>")), "{k}");
        }
        assert!(s.contains("<key>ProcessType</key><string>Background</string>"));
        assert!(s.contains("<string>/Users/x/Library/Logs/ferret/ferretd.log</string>"));
        // plutil validates it when available (macOS).
        let f = std::env::temp_dir().join(format!("ferret-test-{}.plist", std::process::id()));
        std::fs::write(&f, &s).unwrap();
        if let Ok(o) = Command::new("plutil").arg("-lint").arg(&f).output() {
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
        }
        let _ = std::fs::remove_file(f);
    }
}
