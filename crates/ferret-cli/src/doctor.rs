//! `ff doctor`: one ✓/✗ line per thing that can go wrong.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use ferret_core::snapshot;

use crate::{daemon, fmt_bytes, fmt_count};

fn line(ok: bool, msg: &str) -> String {
    format!("{} {msg}", if ok { "✓" } else { "✗" })
}

/// Some(true) = readable, Some(false) = PermissionDenied, None = no probe dir exists.
fn probe_fda(home: &Path) -> Option<bool> {
    ["Library/Mail", "Library/Safari"].iter().find_map(|d| match std::fs::read_dir(home.join(d)) {
        Ok(_) => Some(true),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => Some(false),
        Err(_) => None,
    })
}

/// The FDA report: the ✓/✗ line plus, on ✗, the exact fix.
fn fda_report(granted: Option<bool>, ferretd: &str) -> String {
    match granted {
        Some(true) => line(true, "Full Disk Access granted"),
        None => line(true, "Full Disk Access: could not test (no ~/Library/Mail or ~/Library/Safari)"),
        Some(false) => format!(
            "{}\n  System Settings → Privacy & Security → Full Disk Access → + → add {ferretd}\n  \
             (and add your terminal app too, for `ff index`)\n  \
             hint: open \"x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles\"",
            line(false, "Full Disk Access missing")
        ),
    }
}

fn age(t: SystemTime) -> String {
    let s = t.elapsed().map_or(0, |d| d.as_secs());
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

pub fn run() -> Result<()> {
    let snap = snapshot::default_path();
    match std::fs::metadata(&snap) {
        Ok(m) => println!(
            "{}",
            line(true, &format!(
                "snapshot {} · {} · {} old",
                snap.display(),
                fmt_bytes(m.len()),
                m.modified().map(age).unwrap_or_else(|_| "?".into())
            ))
        ),
        Err(_) => println!("{}", line(false, &format!("snapshot missing at {} — run `ff index`", snap.display()))),
    }

    match daemon::socket_status() {
        Some(s) => println!(
            "{}",
            line(true, &format!(
                "daemon socket reachable · {} files · last event {} · {}",
                fmt_count(s.files),
                s.last_event_id,
                if s.indexing { "indexing" } else { "idle" }
            ))
        ),
        None => println!("{}", line(false, "daemon socket unreachable — `ff daemon install`")),
    }

    let p = daemon::plist_path();
    match (p.exists(), daemon::launchd_state()) {
        (true, Some((true, pid))) => println!("{}", line(true, &format!("launchd agent running (pid {})", pid.unwrap_or_default()))),
        (true, _) => println!("{}", line(false, "launchd agent installed but not running")),
        (false, _) => println!("{}", line(false, "launchd agent not installed — `ff daemon install`")),
    }

    let ferretd = daemon::ferretd_path().unwrap_or_else(|_| PathBuf::from("/path/to/ferretd"));
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    println!("{}", fda_report(probe_fda(&home), &ferretd.display().to_string()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fda_formatting() {
        assert_eq!(fda_report(Some(true), "/x/ferretd"), "✓ Full Disk Access granted");
        let bad = fda_report(Some(false), "/x/ferretd");
        assert!(bad.starts_with("✗ Full Disk Access missing\n"));
        assert!(bad.contains("Full Disk Access → + → add /x/ferretd"));
        assert!(bad.contains("terminal app"));
        assert!(bad.contains(r#"open "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles""#));
        assert!(fda_report(None, "/x").starts_with('✓'));
    }

    #[test]
    fn probe_missing_dirs() {
        assert_eq!(probe_fda(Path::new("/nonexistent-ferret-home")), None);
    }
}
