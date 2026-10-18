//! `ff mcp`: a read-only MCP server that lets AI agents find files by name and metadata.
//! Security boundary: every path that leaves this crate passes `Policy::allows`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ferret_core::protocol::{self, HitOut, KindFilter, Op, SortArg};
use ferret_core::{Index, Kind};
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
}

#[derive(Debug, Clone)]
pub struct Policy {
    allow: Vec<PathBuf>,
    deny: Vec<PathBuf>,
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

/// Lexical form plus the symlink-resolved form when it differs (e.g. /var -> /private/var).
fn root_forms(p: PathBuf) -> Vec<PathBuf> {
    let Some(p) = normalize(&p) else { return vec![] };
    match std::fs::canonicalize(&p) {
        Ok(c) if c != p => vec![p, c],
        _ => vec![p],
    }
}

impl Policy {
    /// Allow defaults to `home`; the default deny list is always added.
    pub fn new(home: &Path, allow: Option<Vec<String>>, deny: Vec<String>) -> Self {
        let allow = allow.unwrap_or_else(|| vec!["~".into()]);
        let deny = DEFAULT_DENY.iter().map(|d| home.join(d)).chain(deny.iter().map(|d| expand_tilde(d, home)));
        Policy {
            allow: allow.iter().flat_map(|a| root_forms(expand_tilde(a, home))).collect(),
            deny: deny.flat_map(root_forms).collect(),
        }
    }

    /// Missing config file = defaults; a broken one is an error (never silently widen or narrow).
    pub fn load(home: &Path, config: &Path) -> anyhow::Result<Self> {
        let cfg: ConfigFile = match std::fs::read_to_string(config) {
            Ok(s) => toml::from_str(&s)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self::new(home, cfg.mcp.allow, cfg.mcp.deny))
    }

    /// Lexical, component-wise check. Deny always wins.
    pub fn allows(&self, p: &Path) -> bool {
        let Some(p) = normalize(p) else { return false };
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
    index: OnceLock<Result<Index, String>>,
}

impl Ctx {
    pub fn new(policy: Policy, home: PathBuf, snapshot: PathBuf, log: PathBuf, daemon: bool) -> Self {
        Ctx { policy, home, snapshot, log, daemon, index: OnceLock::new() }
    }

    fn from_env() -> anyhow::Result<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("$HOME is not set"))?);
        let policy = Policy::load(&home, &home.join(".config/ferret/config.toml"))?;
        let log = home.join("Library/Logs/ferret/mcp.log");
        Ok(Ctx::new(policy, home, ferret_core::snapshot::default_path(), log, true))
    }

    fn engine_search(&self, q: &str, limit: usize, sort: SortArg, kind: KindFilter) -> Result<Vec<HitOut>, String> {
        if self.daemon {
            if let Ok(r) = protocol::request(Op::Search { q: q.to_string(), limit, sort, kind }) {
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
        let index = self
            .index
            .get_or_init(|| {
                ferret_core::snapshot::load(&self.snapshot)
                    .map_err(|e| format!("Ferret daemon is not running and no index snapshot could be read: {e}"))
            })
            .as_ref()?;
        let r = ferret_core::search(index, &query, limit, sort.into());
        Ok(r.hits.into_iter().map(|h| HitOut { path: h.path, kind: h.kind, size: h.size, mtime: h.mtime }).collect())
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
        let limit = limit.unwrap_or(20).clamp(1, MAX_LIMIT);
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

fn filter_hits(policy: &Policy, hits: Vec<HitOut>, under: Option<&Path>, limit: usize) -> Vec<HitOut> {
    let under = under.and_then(normalize);
    hits.into_iter()
        .filter(|h| policy.allows(Path::new(&h.path)))
        .filter(|h| under.as_ref().is_none_or(|u| Path::new(&h.path).starts_with(u)))
        .take(limit.min(MAX_LIMIT))
        .collect()
}

fn log_line(ts: i64, tool: &str, args: &serde_json::Value, count: usize, took_ms: u128) -> String {
    serde_json::json!({ "timestamp": ts, "tool": tool, "args": args, "result_count": count, "took_ms": took_ms })
        .to_string()
}

/// Tokens we splice into a query must stay one token.
fn safe_token(name: &str, v: &str) -> Result<(), String> {
    if v.is_empty() || v.contains(|c: char| c.is_whitespace() || c == '"') {
        return Err(format!("{name} must be a single word without spaces or quotes"));
    }
    Ok(())
}

/// `in:` only when the path tokenizes cleanly; `under` is always re-applied as a post-filter anyway.
fn in_clause(under: Option<&Path>) -> String {
    match under.and_then(|u| u.to_str()) {
        Some(u) if !u.contains(|c: char| c.is_whitespace() || c == '"') => format!(" in:{u}"),
        _ => String::new(),
    }
}

fn recent_query(since: &str, ext: Option<&str>, under: Option<&Path>) -> Result<String, String> {
    safe_token("since", since)?;
    let mut q = format!("type:file mod:<{since}");
    if let Some(e) = ext {
        safe_token("ext", e)?;
        q += &format!(" ext:{e}");
    }
    q += &in_clause(under);
    ferret_core::parse(&q).map_err(|e| e.to_string())?;
    Ok(q)
}

fn largest_query(under: Option<&Path>) -> String {
    format!("type:file{}", in_clause(under))
}

// ---------- formatting ----------

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
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
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

fn render(hits: &[HitOut]) -> String {
    let mut s = format!("{} result{}\n", hits.len(), if hits.len() == 1 { "" } else { "s" });
    for h in hits {
        let slash = if h.kind == Kind::Dir { "/" } else { "" };
        s += &format!("{}{slash}  {}  {}\n", h.path, human_size(h.size), fmt_time(h.mtime));
    }
    s
}

fn file_info(ctx: &Ctx, path: &str) -> Result<String, String> {
    // Same message for missing and denied: no existence oracle for private paths.
    let nope = || format!("{path}: not found or not allowed");
    let lexical = normalize(&expand_tilde(path, &ctx.home)).ok_or_else(|| "path must be absolute or start with ~".to_string())?;
    if !ctx.policy.allows(&lexical) {
        return Err(nope());
    }
    let real = std::fs::canonicalize(&lexical).map_err(|_| nope())?;
    if !ctx.policy.allows(&real) {
        return Err(nope());
    }
    let md = std::fs::metadata(&real).map_err(|_| nope())?;
    let secs = |t: std::io::Result<SystemTime>| {
        t.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| fmt_time(d.as_secs() as i64)).unwrap_or("unknown".into())
    };
    let mut s = format!("path: {}\n", real.display());
    s += &format!("kind: {}\n", if md.is_dir() { "dir" } else { "file" });
    s += &format!("size: {} ({} bytes)\n", human_size(md.len()), md.len());
    s += &format!("created: {}\n", secs(md.created()));
    s += &format!("modified: {}\n", secs(md.modified()));
    if md.is_dir() {
        let n = std::fs::read_dir(&real).map(|r| r.count()).map_err(|_| nope())?;
        s += &format!("children: {n}\n");
    }
    Ok(s)
}

// ---------- MCP server ----------

#[derive(Debug, Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Ferret query, e.g. `invoice *.pdf mod:<30d in:~/Documents`.
    pub query: String,
    /// Max results, 1-100 (default 20).
    pub limit: Option<usize>,
    /// all (default), file or dir.
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct RecentArgs {
    /// Folder to look in, e.g. ~/Documents. Default: everything allowed.
    pub under: Option<String>,
    /// How far back, e.g. 2h, 7d (default 1d).
    pub since: Option<String>,
    /// Only this extension, e.g. pdf.
    pub ext: Option<String>,
    /// Max results, 1-100 (default 20).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct LargestArgs {
    /// Folder to look in, e.g. ~/Downloads. Default: everything allowed.
    pub under: Option<String>,
    /// Max results, 1-100 (default 20).
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
        let r = tokio::task::spawn_blocking(move || f(&ctx)).await.unwrap_or_else(|e| Err(e.to_string()));
        let args = serde_json::to_value(args).unwrap_or_default();
        let count = r.as_ref().map(|r| r.1).unwrap_or(0);
        self.ctx.log_call(tool, &args, count, start.elapsed().as_millis());
        r.map(|r| r.0)
    }
}

fn kind_arg(k: Option<&str>) -> Result<KindFilter, String> {
    match k.unwrap_or("all") {
        "all" => Ok(KindFilter::All),
        "file" => Ok(KindFilter::File),
        "dir" => Ok(KindFilter::Dir),
        o => Err(format!("bad kind '{o}': use all, file or dir")),
    }
}

fn hits_out(hits: Vec<HitOut>) -> (String, usize) {
    (render(&hits), hits.len())
}

#[tool_router]
impl FerretMcp {
    #[tool(description = "Find files and folders by name using Ferret query syntax. Returns paths with size and modified date.")]
    async fn search_files(&self, Parameters(a): Parameters<SearchArgs>) -> Result<String, String> {
        let (q, limit, kind) = (a.query.clone(), a.limit, a.kind.clone());
        self.call("search_files", a, move |ctx| {
            let kind = kind_arg(kind.as_deref())?;
            ctx.filtered(&q, limit, SortArg::Score, kind, None).map(hits_out)
        })
        .await
    }

    #[tool(description = "Recently modified files, newest first.")]
    async fn recent_files(&self, Parameters(a): Parameters<RecentArgs>) -> Result<String, String> {
        let (under, since, ext, limit) = (a.under.clone(), a.since.clone(), a.ext.clone(), a.limit);
        self.call("recent_files", a, move |ctx| {
            let under = under.map(|u| expand_tilde(&u, &ctx.home));
            let q = recent_query(since.as_deref().unwrap_or("1d"), ext.as_deref(), under.as_deref())?;
            ctx.filtered(&q, limit, SortArg::Mtime, KindFilter::File, under.as_deref()).map(hits_out)
        })
        .await
    }

    #[tool(description = "Biggest files first.")]
    async fn largest_files(&self, Parameters(a): Parameters<LargestArgs>) -> Result<String, String> {
        let (under, limit) = (a.under.clone(), a.limit);
        self.call("largest_files", a, move |ctx| {
            let under = under.map(|u| expand_tilde(&u, &ctx.home));
            ctx.filtered(&largest_query(under.as_deref()), limit, SortArg::Size, KindFilter::File, under.as_deref())
                .map(hits_out)
        })
        .await
    }

    #[tool(description = "Kind, size, created/modified dates and child count of one file or folder. Never reads contents.")]
    async fn file_info(&self, Parameters(a): Parameters<InfoArgs>) -> Result<String, String> {
        let path = a.path.clone();
        self.call("file_info", a, move |ctx| file_info(ctx, &path).map(|s| (s, 1))).await
    }
}

// The macro only accepts a string literal here.
#[tool_handler(
    name = "ferret",
    instructions = "Ferret finds files on this Mac by name and metadata (never contents). Query syntax for search_files: plain words must all appear in the name (case-insensitive); \"exact phrase\"; !word excludes; *.pdf or ext:pdf;jpg filters extensions; type:file / type:dir; size:>100MB, size:<1KB; mod:<7d (changed within), mod:>1y (older than), units s m h d w y; in:~/Projects limits to a folder. Results are limited to folders the user allowed; private folders (keys, mail, browsers) are never shown. Dates are UTC."
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
