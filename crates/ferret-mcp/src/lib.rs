//! `ff mcp`: a read-only MCP server that lets AI agents find files by name and metadata,
//! and (opt-in) search their contents.
//! Security boundary: every path that leaves this crate passes `Policy::allows`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ferret_core::protocol::{self, HitOut, KindFilter, Op, SortArg};
use ferret_core::{Excludes, Index, Kind};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::Deserialize;

const MAX_LIMIT: usize = 100;
const MAX_FETCH: usize = 400;

const DEFAULT_DENY: &[&str] = &[
    ".ssh",
    ".gnupg",
    "Library/Keychains",
    "Library/Messages",
    "Library/Mail",
    "Library/Cookies",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Firefox",
    "Library/Safari",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Arc",
    ".aws",
    ".kube",
    ".docker/config.json",
    ".netrc",
    ".config/gh",
];

// ---------- policy ----------

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    mcp: McpConfig,
}

#[derive(Debug, Default, Deserialize)]
struct McpConfig {
    allow: Option<Vec<String>>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    contents: bool,
}

#[derive(Debug, Clone)]
pub struct Policy {
    allow: Vec<PathBuf>,
    deny: Vec<PathBuf>,
    /// search_contents may read files. Off unless `[mcp] contents = true`.
    pub contents: bool,
}

/// Resolve `.` and `..` without touching the disk. Relative paths are rejected.
fn normalize(p: &Path) -> Option<PathBuf> {
    if !p.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::RootDir | Component::Prefix(_) => out.push(c),
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
        }
    }
    Some(out)
}

fn expand_tilde(s: &str, home: &Path) -> PathBuf {
    if s == "~" {
        home.to_path_buf()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(s)
    }
}

/// NFC + lowercase: APFS is case-insensitive and normalization-insensitive by default,
/// so ~/Secrets and ~/secrets (or NFD vs NFC names) are the same folder and must match.
fn fold(p: &Path) -> PathBuf {
    use unicode_normalization::UnicodeNormalization;
    PathBuf::from(p.to_string_lossy().nfc().collect::<String>().to_lowercase())
}

/// Lexical form plus the symlink-resolved form when it differs (e.g. /var -> /private/var), folded.
fn root_forms(p: PathBuf) -> Vec<PathBuf> {
    let Some(p) = normalize(&p) else {
        return vec![];
    };
    match std::fs::canonicalize(&p) {
        Ok(c) if c != p => vec![fold(&p), fold(&c)],
        _ => vec![fold(&p)],
    }
}

impl Policy {
    /// Allow defaults to `home`; the default deny list is always added.
    pub fn new(home: &Path, allow: Option<Vec<String>>, deny: Vec<String>) -> Self {
        let allow = allow.unwrap_or_else(|| vec!["~".into()]);
        let deny = DEFAULT_DENY
            .iter()
            .map(|d| home.join(d))
            .chain(deny.iter().map(|d| expand_tilde(d, home)));
        Policy {
            allow: allow
                .iter()
                .flat_map(|a| root_forms(expand_tilde(a, home)))
                .collect(),
            deny: deny.flat_map(root_forms).collect(),
            contents: false,
        }
    }

    /// Missing config file = defaults; a broken one is an error (never silently widen or narrow).
    pub fn load(home: &Path, config: &Path) -> anyhow::Result<Self> {
        let cfg: ConfigFile = match std::fs::read_to_string(config) {
            Ok(s) => toml::from_str(&s)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Policy {
            contents: cfg.mcp.contents,
            ..Self::new(home, cfg.mcp.allow, cfg.mcp.deny)
        })
    }

    /// Lexical, component-wise, case- and normalization-insensitive check. Deny always wins.
    pub fn allows(&self, p: &Path) -> bool {
        let Some(p) = normalize(p) else { return false };
        let p = fold(&p);
        self.allow.iter().any(|a| p.starts_with(a)) && !self.deny.iter().any(|d| p.starts_with(d))
    }
}

// ---------- engine ----------

pub struct Ctx {
    pub policy: Policy,
    pub home: PathBuf,
    pub snapshot: PathBuf,
    pub log: PathBuf,
    /// Try the daemon socket first. Off in tests.
    pub daemon: bool,
    /// What search_contents skips, like the index. Defaults on.
    pub excludes: Excludes,
    index: OnceLock<Index>,
}

impl Ctx {
    pub fn new(
        policy: Policy,
        home: PathBuf,
        snapshot: PathBuf,
        log: PathBuf,
        daemon: bool,
    ) -> Self {
        Ctx {
            policy,
            home,
            snapshot,
            log,
            daemon,
            excludes: Excludes::new(true, &[], Path::new("/")).expect("no globs"),
            index: OnceLock::new(),
        }
    }

    fn from_env() -> anyhow::Result<Self> {
        let home = PathBuf::from(
            std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("$HOME is not set"))?,
        );
        let policy = Policy::load(&home, &home.join(".config/ferret/config.toml"))?;
        let log = home.join("Library/Logs/ferret/mcp.log");
        let mut ctx = Ctx::new(
            policy,
            home,
            ferret_core::snapshot::default_path(),
            log,
            true,
        );
        ctx.excludes = Excludes::load(&[], false)?;
        Ok(ctx)
    }

    fn engine_search(
        &self,
        q: &str,
        limit: usize,
        sort: SortArg,
        kind: KindFilter,
    ) -> Result<Vec<HitOut>, String> {
        if self.daemon {
            if let Ok(r) = protocol::request(Op::Search {
                q: q.to_string(),
                limit,
                sort,
                kind,
            }) {
                return match r.error {
                    Some(e) => Err(e),
                    None => Ok(r.hits),
                };
            }
        }
        let mut query = ferret_core::parse(q).map_err(|e| e.to_string())?;
        let want = match kind {
            KindFilter::All => None,
            KindFilter::File => Some(Kind::File),
            KindFilter::Dir => Some(Kind::Dir),
        };
        if let Some(w) = want {
            if query.kind.is_some_and(|k| k != w) {
                return Ok(vec![]);
            }
            query.kind = Some(w);
        }
        let index = self.index()?;
        let r = ferret_core::search(index, &query, limit, sort.into());
        Ok(r.hits
            .into_iter()
            .map(|h| HitOut {
                path: h.path,
                kind: h.kind,
                size: h.size,
                mtime: h.mtime,
            })
            .collect())
    }

    /// Cache only a successful load, so a snapshot written later (e.g. by `ff index`) is picked up.
    fn index(&self) -> Result<&Index, String> {
        if let Some(i) = self.index.get() {
            return Ok(i);
        }
        let i = ferret_core::snapshot::load(&self.snapshot).map_err(|e| {
            format!("Ferret daemon is not running and no index snapshot could be read: {e}")
        })?;
        Ok(self.index.get_or_init(|| i))
    }

    /// Over-fetch, then drop anything outside the policy (and outside `under`), then cap.
    fn filtered(
        &self,
        q: &str,
        limit: Option<usize>,
        sort: SortArg,
        kind: KindFilter,
        under: Option<&Path>,
    ) -> Result<Vec<HitOut>, String> {
        let limit = limit.unwrap_or(10).clamp(1, MAX_LIMIT);
        let hits = self.engine_search(q, (limit * 4).min(MAX_FETCH), sort, kind)?;
        Ok(filter_hits(&self.policy, hits, under, limit))
    }

    fn log_call(&self, tool: &str, args: &serde_json::Value, count: usize, took_ms: u128) {
        // Logging must never fail the call.
        let line = log_line(now_secs(), tool, args, count, took_ms);
        if let Some(dir) = self.log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.log) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn filter_hits(
    policy: &Policy,
    hits: Vec<HitOut>,
    under: Option<&Path>,
    limit: usize,
) -> Vec<HitOut> {
    let under = under.and_then(normalize);
    hits.into_iter()
        .filter(|h| policy.allows(Path::new(&h.path)))
        .filter(|h| {
            under
                .as_ref()
                .is_none_or(|u| Path::new(&h.path).starts_with(u))
        })
        .take(limit.min(MAX_LIMIT))
        .collect()
}

fn log_line(ts: i64, tool: &str, args: &serde_json::Value, count: usize, took_ms: u128) -> String {
    serde_json::json!({ "timestamp": ts, "tool": tool, "args": args, "result_count": count, "took_ms": took_ms })
        .to_string()
}

// ---------- formatting ----------

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// shortcut: UTC (no tz database), switch to local time if users complain.
fn fmt_time(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}

fn tilde(p: &str, home: &Path) -> String {
    match p.strip_prefix(home.to_string_lossy().as_ref()) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => p.to_owned(),
    }
}

/// One line per hit, home shown as `~` (every token here is re-sent on later turns).
fn render(hits: &[HitOut], home: &Path) -> String {
    if hits.is_empty() {
        return "no matches".into();
    }
    let mut s = String::new();
    for h in hits {
        let path = tilde(&h.path, home);
        let slash = if h.kind == Kind::Dir { "/" } else { "" };
        s += &format!(
            "{path}{slash}  {}  {}\n",
            human_size(h.size),
            fmt_time(h.mtime)
        );
    }
    s
}

fn file_info(ctx: &Ctx, path: &str) -> Result<String, String> {
    // Same message for missing and denied: no existence oracle for private paths.
    let nope = || format!("{path}: not found or not allowed");
    let lexical = normalize(&expand_tilde(path, &ctx.home))
        .ok_or_else(|| "path must be absolute or start with ~".to_string())?;
    if !ctx.policy.allows(&lexical) {
        return Err(nope());
    }
    let real = std::fs::canonicalize(&lexical).map_err(|_| nope())?;
    if !ctx.policy.allows(&real) {
        return Err(nope());
    }
    let md = std::fs::metadata(&real).map_err(|_| nope())?;
    let secs = |t: std::io::Result<SystemTime>| {
        t.ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| fmt_time(d.as_secs() as i64))
            .unwrap_or("unknown".into())
    };
    let mut s = format!("path: {}\n", real.display());
    s += &format!("kind: {}\n", if md.is_dir() { "dir" } else { "file" });
    s += &format!("size: {} ({} bytes)\n", human_size(md.len()), md.len());
    s += &format!("created: {}\n", secs(md.created()));
    s += &format!("modified: {}\n", secs(md.modified()));
    if md.is_dir() {
        let n = std::fs::read_dir(&real)
            .map(|r| r.count())
            .map_err(|_| nope())?;
        s += &format!("children: {n}\n");
    }
    Ok(s)
}

const CONTENTS_OFF: &str =
    "Content search is off. Turn it on in Ferret Settings → AI agents, or set `contents = true` under [mcp] in ~/.config/ferret/config.toml.";
const MAX_CONTENT_LIMIT: usize = 200;
const MAX_CONTENT_FILE: u64 = 5 << 20;

/// grep over allowed files under `path`: same excludes and .gitignore as the index,
/// no binaries, nothing over 5 MB, symlinks not followed. Out: `~/path:line: text`.
fn search_contents(ctx: &Ctx, a: &ContentArgs) -> Result<(String, usize), String> {
    use grep_regex::RegexMatcherBuilder;
    use grep_searcher::{sinks::Lossy, BinaryDetection, SearcherBuilder};

    if !ctx.policy.contents {
        return Err(CONTENTS_OFF.into());
    }
    let shown = a.path.as_deref().unwrap_or("~");
    let nope = || format!("{shown}: not found or not allowed");
    let lexical = normalize(&expand_tilde(shown, &ctx.home))
        .ok_or_else(|| "path must be absolute or start with ~".to_string())?;
    if !ctx.policy.allows(&lexical) {
        return Err(nope());
    }
    let root = std::fs::canonicalize(&lexical).map_err(|_| nope())?;
    if !ctx.policy.allows(&root) {
        return Err(nope());
    }
    let limit = a.limit.unwrap_or(50).clamp(1, MAX_CONTENT_LIMIT);
    // Not a valid regex (e.g. `foo(`): search for it literally.
    let matcher = RegexMatcherBuilder::new()
        .case_smart(true)
        .build(&a.query)
        .or_else(|_| RegexMatcherBuilder::new().case_smart(true).fixed_strings(true).build(&a.query))
        .map_err(|e| format!("bad query: {e}"))?;

    let ex = ctx.excludes.clone();
    let policy = ctx.policy.clone();
    let mut wb = ignore::WalkBuilder::new(&root);
    wb.hidden(false)
        .ignore(false)
        .git_global(false)
        .git_ignore(ex.defaults)
        .git_exclude(ex.defaults)
        .max_filesize(Some(MAX_CONTENT_FILE));
    if let Some(g) = &a.glob {
        let mut o = ignore::overrides::OverrideBuilder::new(&root);
        o.add(g).map_err(|e| format!("bad glob: {e}"))?;
        wb.overrides(o.build().map_err(|e| format!("bad glob: {e}"))?);
    }
    wb.filter_entry(move |e| {
        let p = e.path();
        let is_dir = e.file_type().is_some_and(|t| t.is_dir());
        let skipped = e.depth() > 0
            && p.parent()
                .is_some_and(|d| ex.skip(d, &e.file_name().to_string_lossy(), is_dir));
        policy.allows(p)
            && !skipped
            && !(ex.defaults && is_dir && p.join("pyvenv.cfg").exists())
    });

    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .build();
    let mut out = String::new();
    let mut n = 0;
    for ent in wb.build().flatten() {
        if !ent.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = tilde(&ent.path().to_string_lossy(), &ctx.home);
        let sink = Lossy(|ln, line: &str| {
            let text: String = line.trim().chars().take(200).collect();
            out += &format!("{path}:{ln}: {text}\n");
            n += 1;
            Ok(n < limit)
        });
        let _ = searcher.search_path(&matcher, ent.path(), sink); // unreadable file: skip it
        if n >= limit {
            break;
        }
    }
    if n == 0 {
        out = "no matches".into();
    }
    Ok((out, n))
}

// ---------- MCP server ----------

#[derive(Debug, Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// e.g. `invoice *.pdf mod:<30d in:~/Documents`
    pub query: String,
    /// relevance (default), size, newest
    pub sort: Option<String>,
    /// 1-100, default 10
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct ContentArgs {
    /// Regex, or a literal if it isn't valid regex. Smart case.
    pub query: String,
    /// Folder to search, absolute or ~/path. Default ~
    #[serde(rename = "in")]
    pub path: Option<String>,
    /// File glob, e.g. `*.rs`
    pub glob: Option<String>,
    /// 1-200 matching lines, default 50
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct InfoArgs {
    /// Absolute path or ~/path.
    pub path: String,
}

#[derive(Clone)]
pub struct FerretMcp {
    ctx: Arc<Ctx>,
}

impl FerretMcp {
    pub fn new(ctx: Ctx) -> Self {
        FerretMcp { ctx: Arc::new(ctx) }
    }

    /// Run blocking work off the async runtime, log it, map output to text.
    async fn call<A: serde::Serialize>(
        &self,
        tool: &'static str,
        args: A,
        f: impl FnOnce(&Ctx) -> Result<(String, usize), String> + Send + 'static,
    ) -> Result<String, String> {
        let start = Instant::now();
        let ctx = self.ctx.clone();
        let r = tokio::task::spawn_blocking(move || f(&ctx))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        let args = serde_json::to_value(args).unwrap_or_default();
        let count = r.as_ref().map(|r| r.1).unwrap_or(0);
        self.ctx
            .log_call(tool, &args, count, start.elapsed().as_millis());
        r.map(|r| r.0)
    }
}

fn sort_arg(s: Option<&str>) -> Result<SortArg, String> {
    match s.unwrap_or("relevance") {
        "relevance" => Ok(SortArg::Score),
        "size" => Ok(SortArg::Size),
        "newest" => Ok(SortArg::Mtime),
        o => Err(format!("bad sort '{o}': use relevance, size or newest")),
    }
}

/// Claude Code defers MCP tools behind ToolSearch, costing a whole extra turn; this opts out.
fn always_load() -> rmcp::model::MetaObject {
    let mut m = rmcp::model::MetaObject::new();
    m.insert("anthropic/alwaysLoad".into(), true.into());
    m
}

#[tool_router]
impl FerretMcp {
    // Only this tool is loaded up front: its schema is re-sent on every turn.
    #[tool(
        meta = always_load(),
        description = "Find files/folders on this Mac by name, recursively. Query: words (all must match), \"phrase\", !exclude, *.pdf, type:file|dir, size:>100MB, mod:<7d (changed within; s m h d w y), in:~/folder. Out: path size modified(UTC)."
    )]
    async fn find_files(&self, Parameters(a): Parameters<SearchArgs>) -> Result<String, String> {
        let (q, limit, sort) = (a.query.clone(), a.limit, a.sort.clone());
        self.call("find_files", a, move |ctx| {
            let sort = sort_arg(sort.as_deref())?;
            let hits = ctx.filtered(&q, limit, sort, KindFilter::All, None)?;
            Ok((render(&hits, &ctx.home), hits.len()))
        })
        .await
    }

    #[tool(
        description = "Search inside files (like grep -rn). Respects .gitignore; skips node_modules, venvs, build dirs, binaries and files over 5 MB. Out: path:line: text."
    )]
    async fn search_contents(&self, Parameters(a): Parameters<ContentArgs>) -> Result<String, String> {
        self.call("search_contents", a.clone(), move |ctx| search_contents(ctx, &a))
            .await
    }

    #[tool(
        description = "Kind, size, created/modified dates and child count of one file or folder. Never reads contents."
    )]
    async fn file_info(&self, Parameters(a): Parameters<InfoArgs>) -> Result<String, String> {
        let path = a.path.clone();
        self.call("file_info", a, move |ctx| {
            file_info(ctx, &path).map(|s| (s, 1))
        })
        .await
    }
}

// The macro only accepts a string literal here.
#[tool_handler(
    name = "ferret",
    instructions = "find_files = search by name (instant, whole Mac; prefer it over find/mdfind/ls). search_contents = search inside files (prefer it over grep/rg). file_info = metadata of one path."
)]
impl ServerHandler for FerretMcp {}

/// Serve MCP over stdio until EOF.
pub async fn run() -> anyhow::Result<()> {
    serve(FerretMcp::new(Ctx::from_env()?), rmcp::transport::stdio()).await
}

async fn serve<T, E, A>(server: FerretMcp, transport: T) -> anyhow::Result<()>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    server.serve(transport).await?.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
