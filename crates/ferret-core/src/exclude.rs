//! What the scanner leaves out: built-in junk folders, virtualenvs, .gitignore'd paths and
//! user globs from `exclude = [...]` / `[app] excluded` in ~/.config/ferret/config.toml.
use crate::{Error, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Folder names skipped anywhere unless `default_excludes = false`.
pub const DEFAULT_DIRS: &[&str] = &[
    "node_modules",
    ".venv",
    ".venv2",
    "venv",
    "env",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".vite",
    "coverage",
    "htmlcov",
    ".gradle",
    "Pods",
    ".dart_tool",
    ".git",
    ".idea",
];

#[derive(Debug, Clone, Default)]
pub struct Excludes {
    /// DEFAULT_DIRS, `pyvenv.cfg` folders and .gitignore files.
    pub defaults: bool,
    globs: GlobSet,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    exclude: Vec<String>,
    default_excludes: Option<bool>,
    #[serde(default)]
    app: AppSection,
}

#[derive(Debug, Default, Deserialize)]
struct AppSection {
    #[serde(default)]
    excluded: Vec<String>,
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

pub fn config_path() -> PathBuf {
    home().join(".config/ferret/config.toml")
}

impl Excludes {
    /// Index everything.
    pub fn none() -> Self {
        Self::default()
    }

    /// Globs match a full path or a bare name; `~/` is expanded.
    pub fn new(defaults: bool, patterns: &[String], home: &Path) -> Result<Self> {
        let mut b = GlobSetBuilder::new();
        for p in patterns {
            let p = match p.strip_prefix("~/") {
                Some(rest) => home.join(rest).to_string_lossy().into_owned(),
                None => p.clone(),
            };
            let g = GlobBuilder::new(p.trim_end_matches('/'))
                .literal_separator(true)
                .build()
                .map_err(|e| Error::Config(e.to_string()))?;
            b.add(g);
        }
        let globs = b.build().map_err(|e| Error::Config(e.to_string()))?;
        Ok(Excludes { defaults, globs })
    }

    /// Config file plus `ff index --exclude` globs. A missing file means the defaults;
    /// a broken one is an error rather than a silently different index.
    pub fn from_toml(text: &str, home: &Path, extra: &[String], no_defaults: bool) -> Result<Self> {
        let cfg: ConfigFile = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        let mut pats = cfg.exclude;
        pats.extend(cfg.app.excluded);
        pats.extend_from_slice(extra);
        Self::new(
            !no_defaults && cfg.default_excludes != Some(false),
            &pats,
            home,
        )
    }

    pub fn load(extra: &[String], no_defaults: bool) -> Result<Self> {
        let text = match std::fs::read_to_string(config_path()) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        Self::from_toml(&text, &home(), extra, no_defaults)
    }

    /// Is the entry `name` inside `dir` excluded by name or glob? (.gitignore and venvs are
    /// handled by the scanner, which has the directory listing.)
    pub fn skip(&self, dir: &Path, name: &str, is_dir: bool) -> bool {
        (self.defaults && is_dir && DEFAULT_DIRS.contains(&name))
            || (!self.globs.is_empty()
                && (self.globs.is_match(name) || self.globs.is_match(dir.join(name))))
    }

    pub fn is_active(&self) -> bool {
        self.defaults || !self.globs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(toml: &str, extra: &[&str]) -> Excludes {
        let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        Excludes::from_toml(toml, Path::new("/Users/me"), &extra, false).unwrap()
    }

    #[test]
    fn defaults_skip_dirs_not_files() {
        let e = ex("", &[]);
        assert!(e.skip(Path::new("/p"), "node_modules", true));
        assert!(e.skip(Path::new("/p"), ".venv2", true));
        assert!(!e.skip(Path::new("/p"), "node_modules", false));
        assert!(!e.skip(Path::new("/p"), "src", true));
    }

    #[test]
    fn defaults_can_be_turned_off() {
        let e = ex("default_excludes = false", &[]);
        assert!(!e.skip(Path::new("/p"), "node_modules", true));
        assert!(!e.is_active());
        let e = Excludes::from_toml("", Path::new("/"), &[], true).unwrap();
        assert!(!e.defaults);
    }

    #[test]
    fn user_globs_from_config_app_and_cli() {
        let e = ex(
            "exclude = [\"*.log\", \"~/Library/Caches\"]\n[app]\nexcluded = [\"target\"]",
            &["/tmp/**"],
        );
        assert!(e.skip(Path::new("/p"), "x.log", false));
        assert!(e.skip(Path::new("/Users/me/Library"), "Caches", true));
        assert!(!e.skip(Path::new("/Users/me"), "Library", true));
        assert!(e.skip(Path::new("/p"), "target", true));
        assert!(e.skip(Path::new("/tmp/a"), "b", false));
        assert!(!e.skip(Path::new("/p"), "x.txt", false));
    }

    #[test]
    fn broken_config_is_an_error() {
        assert!(Excludes::from_toml("exclude = 3", Path::new("/"), &[], false).is_err());
        assert!(Excludes::from_toml("exclude = [\"a[\"]", Path::new("/"), &[], false).is_err());
    }
}
