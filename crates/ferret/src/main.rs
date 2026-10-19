mod daemon;
mod doctor;
mod picker;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use ferret_core::protocol::{self, HitOut, KindFilter, Op};
use ferret_core::{snapshot, Index, Kind, Node};

const ACCENT: &str = "\x1b[1;38;5;208m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Build the file index.
#[derive(Parser, Debug)]
#[command(name = "ff index")]
struct IndexArgs {
    #[arg(long, default_value = "/")]
    root: PathBuf,
    #[arg(long)]
    stats: bool,
    /// Skip paths matching this glob (repeatable), on top of `exclude` in config.toml.
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,
    /// Also index node_modules, virtualenvs, .gitignore'd files and other default excludes.
    #[arg(long)]
    no_default_excludes: bool,
}

/// Manage the background ferretd launchd agent.
#[derive(Parser, Debug)]
#[command(name = "ff daemon")]
struct DaemonArgs {
    #[arg(value_parser = ["install", "uninstall", "status"])]
    action: String,
}

/// Print shell integration (Ctrl-T picker, `fcd`).
#[derive(Parser, Debug)]
#[command(name = "ff init")]
struct InitArgs {
    shell: String,
}

/// Instant file search. Subcommands: index, daemon, doctor, init, big, recent, mcp.
/// Anything else is a query; `ff -- big` searches for the word "big".
#[derive(Parser, Debug)]
#[command(name = "ff", version)]
struct QueryArgs {
    #[arg(required_unless_present = "interactive")]
    query: Vec<String>,
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[arg(long, value_enum, default_value_t = SortArg::Score)]
    sort: SortArg,
    /// Interactive picker; prints the chosen path.
    #[arg(short, long)]
    interactive: bool,
    /// Start the picker on the Folders tab.
    #[arg(long)]
    dirs: bool,
    #[command(flatten)]
    out: OutArgs,
}

#[derive(clap::Args, Debug)]
struct OutArgs {
    /// One JSON object: {total, took_us, hits:[{path,kind,size,mtime}]}.
    #[arg(long)]
    json: bool,
    /// NUL-separated paths, no status line.
    #[arg(short = '0', long = "null")]
    null: bool,
}

/// `ff big` (largest files) / `ff recent` (newest files, default mod:<1d).
#[derive(Parser, Debug)]
struct ListArgs {
    query: Vec<String>,
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[command(flatten)]
    out: OutArgs,
}

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
enum SortArg {
    Score,
    Size,
    Mtime,
}

#[derive(Debug)]
enum Cmd {
    Index(IndexArgs),
    Daemon(DaemonArgs),
    Doctor,
    Init(InitArgs),
    Big(ListArgs),
    Recent(ListArgs),
    Mcp,
    Query(QueryArgs),
}

/// Manual argv[1] dispatch: no clap subcommands, so query words never collide with flags.
fn dispatch(argv: &[String]) -> Result<Cmd, clap::Error> {
    Ok(match argv.get(1).map(String::as_str) {
        Some("index") => Cmd::Index(IndexArgs::try_parse_from(&argv[1..])?),
        Some("daemon") => Cmd::Daemon(DaemonArgs::try_parse_from(&argv[1..])?),
        Some("doctor") => Cmd::Doctor,
        Some("init") => Cmd::Init(InitArgs::try_parse_from(&argv[1..])?),
        Some("big") => Cmd::Big(ListArgs::try_parse_from(&argv[1..])?),
        Some("recent") => Cmd::Recent(ListArgs::try_parse_from(&argv[1..])?),
        Some("mcp") => Cmd::Mcp,
        _ => Cmd::Query(QueryArgs::try_parse_from(argv)?),
    })
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    match dispatch(&argv).unwrap_or_else(|e| e.exit()) {
        Cmd::Index(a) => run_index(a),
        Cmd::Daemon(a) => daemon::run(&a.action),
        Cmd::Doctor => doctor::run(),
        Cmd::Init(a) => match zsh_init(&a.shell) {
            Some(s) => {
                print!("{s}");
                Ok(())
            }
            None => {
                eprintln!("ff init {}: not yet supported (only zsh)", a.shell);
                std::process::exit(1);
            }
        },
        Cmd::Big(a) => {
            let text = format!("type:file {}", a.query.join(" "));
            run_search(
                &text,
                a.limit,
                protocol::SortArg::Size,
                &a.out,
                Some(Line::Size),
            )
        }
        Cmd::Recent(a) => {
            let text = recent_query(&a.query);
            run_search(
                &text,
                a.limit,
                protocol::SortArg::Mtime,
                &a.out,
                Some(Line::Age),
            )
        }
        Cmd::Mcp => tokio::runtime::Runtime::new()?.block_on(ferret_mcp::run()),
        Cmd::Query(a) if a.interactive => picker::run(&a.query.join(" "), a.dirs),
        Cmd::Query(a) => {
            let sort = match a.sort {
                SortArg::Score => protocol::SortArg::Score,
                SortArg::Size => protocol::SortArg::Size,
                SortArg::Mtime => protocol::SortArg::Mtime,
            };
            run_search(&a.query.join(" "), a.limit, sort, &a.out, None)
        }
    }
}

/// Files only, newest first; default window mod:<1d unless the user gave a mod: filter.
fn recent_query(words: &[String]) -> String {
    let m = if words.iter().any(|w| w.starts_with("mod:")) {
        ""
    } else {
        "mod:<1d "
    };
    format!("type:file {m}{}", words.join(" "))
}

/// Per-line prefix for `ff big` / `ff recent`.
#[derive(Clone, Copy)]
enum Line {
    Size,
    Age,
}

/// 0..59s "12s ago", then m/h/d.
fn ago(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

fn zsh_init(shell: &str) -> Option<&'static str> {
    (shell == "zsh").then_some(
        r#"# ferret: Ctrl-T inserts a path picked with `ff -i`; `fcd` jumps to a folder.
ferret-file-widget() {
  local word=${LBUFFER##* } sel
  sel=$(ff -i -- "$word" < /dev/tty) || { zle reset-prompt; return 0; }
  LBUFFER="${LBUFFER%$word}${(q)sel}"
  zle reset-prompt
}
zle -N ferret-file-widget
bindkey '^T' ferret-file-widget
fcd() { local d; d=$(ff -i --dirs "$*") && cd "$d"; }
"#,
    )
}

/// Search results in the shape both paths (daemon / in-process) produce.
#[derive(serde::Serialize)]
pub struct Found {
    pub total: usize,
    pub took_us: u64,
    pub hits: Vec<HitOut>,
}

/// Ask the daemon. Outer Err = daemon unreachable; inner Err = daemon rejected the query.
pub fn search_daemon(
    q: &str,
    limit: usize,
    sort: protocol::SortArg,
    kind: KindFilter,
) -> std::io::Result<Result<Found>> {
    let r = protocol::request(Op::Search {
        q: q.to_string(),
        limit,
        sort,
        kind,
    })?;
    Ok(match r.error {
        Some(e) => Err(anyhow::anyhow!(e)),
        None => Ok(Found {
            hits: r.hits,
            total: r.total,
            took_us: r.took_us,
        }),
    })
}

pub fn search_local(
    index: &Index,
    q: &str,
    limit: usize,
    sort: protocol::SortArg,
    kind: KindFilter,
) -> Result<Found> {
    let mut q = ferret_core::parse(q).map_err(|e| anyhow::anyhow!("{e}"))?;
    match kind {
        KindFilter::All => {}
        KindFilter::File => q.kind = Some(Kind::File),
        KindFilter::Dir => q.kind = Some(Kind::Dir),
    }
    let r = ferret_core::search(index, &q, limit, sort.into());
    let hits = r
        .hits
        .into_iter()
        .map(|h| HitOut {
            path: h.path,
            kind: h.kind,
            size: h.size,
            mtime: h.mtime,
        })
        .collect();
    Ok(Found {
        hits,
        total: r.total,
        took_us: r.took_us,
    })
}

pub fn load_snapshot() -> Result<Index> {
    let path = snapshot::default_path();
    if !path.exists() {
        bail!("no index yet — run `ff index` first");
    }
    snapshot::load(&path).with_context(|| {
        format!(
            "could not read index at {} (try `ff index` again)",
            path.display()
        )
    })
}

fn run_index(a: IndexArgs) -> Result<()> {
    // Taken before scanning, so ferretd replays every change made during and after the scan.
    let meta = snapshot::Meta {
        last_event_id: unsafe { objc2_core_services::FSEventsGetCurrentEventId() },
        volume_uuid: String::new(),
    };
    let ex = ferret_core::Excludes::load(&a.exclude, a.no_default_excludes)
        .context("could not read ~/.config/ferret/config.toml")?;
    let (index, st) = ferret_core::scan::scan_with_progress(&a.root, &ex, |_, _| {})
        .with_context(|| format!("could not scan {}", a.root.display()))?;
    let path = snapshot::default_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    snapshot::save_with(&index, &meta, &path)
        .with_context(|| format!("could not save snapshot to {}", path.display()))?;
    let disk = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    println!(
        "indexed {} files in {:.2?}",
        fmt_count(st.files),
        st.elapsed
    );
    if a.stats {
        let mem = index.nodes.len() * std::mem::size_of::<Node>()
            + index.names.len()
            + index.names_lower.len()
            + index.lower_spans.len() * std::mem::size_of::<(u32, u16)>();
        println!("  files              {}", st.files);
        println!("  dirs               {}", st.dirs);
        println!("  symlinks           {}", st.symlinks);
        println!("  permission denied  {}", st.permission_denied);
        println!("  scan time          {:.2?}", st.elapsed);
        println!("  nodes              {}", index.nodes.len());
        println!("  approx memory      {}", fmt_bytes(mem as u64));
        println!(
            "  snapshot           {} ({})",
            fmt_bytes(disk),
            path.display()
        );
    }
    if st.permission_denied > 0 {
        eprintln!(
            "note: {} folders were unreadable. Grant your terminal Full Disk Access in \
             System Settings → Privacy & Security → Full Disk Access, then re-run `ff index`.",
            st.permission_denied
        );
    }
    Ok(())
}

fn run_search(
    text: &str,
    limit: usize,
    sort: protocol::SortArg,
    o: &OutArgs,
    prefix: Option<Line>,
) -> Result<()> {
    let q = ferret_core::parse(text).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (found, indexed) = match search_daemon(text, limit, sort, KindFilter::All) {
        Ok(r) => (r?, None),
        Err(_) => {
            let index = load_snapshot().unwrap_or_else(|e| {
                eprintln!("{e:#}");
                std::process::exit(1);
            });
            (
                search_local(&index, text, limit, sort, KindFilter::All)?,
                Some(index.nodes.len()),
            )
        }
    };

    let mut out = std::io::stdout().lock();
    if o.json {
        let _ = writeln!(out, "{}", serde_json::to_string(&found)?);
        return Ok(());
    }
    if o.null {
        for h in &found.hits {
            if write!(out, "{}\0", h.path).is_err() {
                return Ok(());
            }
        }
        return Ok(());
    }
    let tty = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    for h in &found.hits {
        let path = if tty {
            highlight(&tilde(&h.path), &q.words)
        } else {
            h.path.clone()
        };
        let line = match prefix {
            None => path,
            Some(Line::Size) => format!("{:>10}  {path}", fmt_bytes(h.size)),
            Some(Line::Age) => format!("{:>8}  {path}", ago(now - h.mtime)),
        };
        if writeln!(out, "{line}").is_err() {
            return Ok(()); // broken pipe (e.g. `| head`)
        }
    }
    if tty {
        let tail = indexed.map_or(" · daemon".to_string(), |n| {
            format!(" · {} files indexed", fmt_count(n as u64))
        });
        eprintln!(
            "{DIM}{} matches · {}{tail}{RESET}",
            found.total,
            fmt_ms(found.took_us)
        );
    }
    Ok(())
}

/// `/Users/me/x` -> `~/x` for display on a terminal; pipes keep absolute paths.
pub fn tilde(path: &str) -> String {
    tilde_with(path, &std::env::var("HOME").unwrap_or_default())
}

fn tilde_with(path: &str, home: &str) -> String {
    if home.is_empty() || home == "/" {
        return path.to_string();
    }
    // The index may store the /private-resolved form of a /tmp or /var home.
    for h in [home.to_string(), format!("/private{home}")] {
        if let Some(rest) = path.strip_prefix(h.as_str()).filter(|r| r.starts_with('/')) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

/// Split at the file name; mark bytes of the name that match any query word (ASCII case-insensitive).
pub fn match_mask<'a>(path: &'a str, words: &[String]) -> (&'a str, &'a str, Vec<bool>) {
    let cut = path.trim_end_matches('/').rfind('/').map_or(0, |i| i + 1);
    let (parent, name) = path.split_at(cut);
    // ASCII lowercasing keeps byte offsets aligned with the original string.
    let lower = name.to_ascii_lowercase();
    let mut hit = vec![false; name.len()];
    for w in words.iter().filter(|w| !w.is_empty()) {
        let w = w.to_ascii_lowercase();
        let mut from = 0;
        while let Some(i) = lower[from..].find(&w) {
            hit[from + i..from + i + w.len()]
                .iter_mut()
                .for_each(|b| *b = true);
            from += i + w.len();
        }
    }
    (parent, name, hit)
}

/// Runs of (text, highlighted) over the name.
pub fn runs<'a>(name: &'a str, hit: &[bool]) -> Vec<(&'a str, bool)> {
    let mut v = Vec::new();
    let mut i = 0;
    while i < name.len() {
        let on = hit[i];
        let j = (i..name.len())
            .find(|&k| hit[k] != on)
            .unwrap_or(name.len());
        v.push((&name[i..j], on));
        i = j;
    }
    v
}

/// Dim the parent folder, bold+accent each query word inside the file name.
fn highlight(path: &str, words: &[String]) -> String {
    let (parent, name, hit) = match_mask(path, words);
    let mut s = String::new();
    if !parent.is_empty() {
        s.push_str(&format!("{DIM}{parent}{RESET}"));
    }
    for (t, on) in runs(name, &hit) {
        if on {
            s.push_str(&format!("{ACCENT}{t}{RESET}"));
        } else {
            s.push_str(t);
        }
    }
    s
}

/// 2_100_000 -> "2.1M", 840_000 -> "840K", 999 -> "999".
pub fn fmt_count(n: u64) -> String {
    let f = |v: f64, u: &str| {
        let s = if v < 10.0 {
            format!("{v:.1}")
        } else {
            format!("{v:.0}")
        };
        format!("{}{u}", s.trim_end_matches(".0"))
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => f(n as f64 / 1e3, "K"),
        _ => f(n as f64 / 1e6, "M"),
    }
}

pub fn fmt_bytes(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.1} KiB", n as f64 / 1024.0),
        1_048_576..=1_073_741_823 => format!("{:.1} MiB", n as f64 / 1_048_576.0),
        _ => format!("{:.1} GiB", n as f64 / 1_073_741_824.0),
    }
}

/// 412 -> "0.4 ms", 12_345 -> "12 ms"
pub fn fmt_ms(us: u64) -> String {
    if us < 100 {
        "<0.1 ms".into()
    } else if us < 10_000 {
        format!("{:.1} ms", us as f64 / 1000.0)
    } else {
        format!("{} ms", us / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn counts() {
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(840_000), "840K");
        assert_eq!(fmt_count(2_100_000), "2.1M");
        assert_eq!(fmt_count(1_000), "1K");
        assert_eq!(fmt_count(12_400_000), "12M");
    }

    #[test]
    fn sizes_and_times() {
        assert_eq!(fmt_bytes(9 << 30), "9.0 GiB");
        assert_eq!(fmt_bytes(1200 << 20), "1.2 GiB");
        assert_eq!(fmt_bytes(5 << 20), "5.0 MiB");
        assert_eq!(fmt_ms(412), "0.4 ms");
        assert_eq!(fmt_ms(7), "<0.1 ms");
        assert_eq!(fmt_ms(12_345), "12 ms");
    }

    #[test]
    fn tilde_shortens_home_only() {
        assert_eq!(tilde_with("/Users/me/a.pdf", "/Users/me"), "~/a.pdf");
        assert_eq!(tilde_with("/Users/meow/a", "/Users/me"), "/Users/meow/a");
        assert_eq!(tilde_with("/private/tmp/h/x", "/tmp/h"), "~/x");
        assert_eq!(tilde_with("/etc/hosts", ""), "/etc/hosts");
    }

    #[test]
    fn highlights_name_only() {
        let s = highlight("/foo/Foo-bar.txt", &["foo".into()]);
        assert_eq!(s, format!("{DIM}/foo/{RESET}{ACCENT}Foo{RESET}-bar.txt"));
        assert_eq!(highlight("x", &[]), "x");
    }

    #[test]
    fn query_flags_after_words() {
        let Cmd::Query(a) = dispatch(&argv("ff old invoice --limit 5 --sort mtime")).unwrap()
        else {
            panic!()
        };
        assert_eq!(a.query, ["old", "invoice"]);
        assert_eq!(a.limit, 5);
        assert_eq!(a.sort, SortArg::Mtime);
        assert!(!a.interactive);
    }

    #[test]
    fn interactive_without_query() {
        let Cmd::Query(a) = dispatch(&argv("ff -i --dirs")).unwrap() else {
            panic!()
        };
        assert!(a.interactive && a.dirs && a.query.is_empty());
        assert!(dispatch(&argv("ff")).is_err());
    }

    #[test]
    fn subcommands() {
        assert!(
            matches!(dispatch(&argv("ff index --root /tmp")).unwrap(), Cmd::Index(a) if a.root.as_os_str() == "/tmp")
        );
        assert!(matches!(
            dispatch(&argv("ff index --exclude *.log --exclude target --no-default-excludes")).unwrap(),
            Cmd::Index(a) if a.exclude == ["*.log", "target"] && a.no_default_excludes
        ));
        assert!(
            matches!(dispatch(&argv("ff daemon status")).unwrap(), Cmd::Daemon(a) if a.action == "status")
        );
        assert!(dispatch(&argv("ff daemon bogus")).is_err());
        assert!(matches!(dispatch(&argv("ff doctor")).unwrap(), Cmd::Doctor));
        // `--` searches for a word that is also a subcommand name
        assert!(
            matches!(dispatch(&argv("ff -- big pdf")).unwrap(), Cmd::Query(a) if a.query == ["big", "pdf"])
        );
        assert!(
            matches!(dispatch(&argv("ff init zsh")).unwrap(), Cmd::Init(a) if a.shell == "zsh")
        );
    }

    #[test]
    fn output_and_list_commands() {
        let Cmd::Query(a) = dispatch(&argv("ff pdf --json")).unwrap() else {
            panic!()
        };
        assert!(a.out.json && a.query == ["pdf"]);
        let Cmd::Query(a) = dispatch(&argv("ff pdf -0")).unwrap() else {
            panic!()
        };
        assert!(a.out.null);
        let Cmd::Big(a) = dispatch(&argv("ff big in:~/Downloads --limit 5 --null")).unwrap() else {
            panic!()
        };
        assert_eq!(
            (a.query.as_slice(), a.limit, a.out.null),
            (&["in:~/Downloads".to_string()][..], 5, true)
        );
        let Cmd::Recent(a) = dispatch(&argv("ff recent --json")).unwrap() else {
            panic!()
        };
        assert_eq!((a.limit, a.out.json), (20, true));
        assert!(matches!(dispatch(&argv("ff mcp")).unwrap(), Cmd::Mcp));
    }

    #[test]
    fn recent_defaults_to_one_day() {
        assert_eq!(recent_query(&[]), "type:file mod:<1d ");
        assert_eq!(
            recent_query(&["mod:<7d".into(), "pdf".into()]),
            "type:file mod:<7d pdf"
        );
        assert!(ferret_core::parse(&recent_query(&["x".into()])).is_ok());
        assert_eq!(ago(30), "30s ago");
        assert_eq!(ago(180), "3m ago");
        assert_eq!(ago(7200), "2h ago");
        assert_eq!(ago(3 * 86_400), "3d ago");
    }

    #[test]
    fn json_shape() {
        let f = Found {
            total: 1,
            took_us: 7,
            hits: vec![HitOut {
                path: "/a".into(),
                kind: Kind::File,
                size: 3,
                mtime: 9,
            }],
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(v["total"], 1);
        assert_eq!(v["took_us"], 7);
        assert_eq!(v["hits"][0]["path"], "/a");
        assert_eq!(v["hits"][0]["size"], 3);
    }

    #[test]
    fn zsh_snippet() {
        let s = zsh_init("zsh").unwrap();
        assert!(s.contains("bindkey '^T'") && s.contains("fcd()") && s.contains("${(q)sel}"));
        assert!(zsh_init("bash").is_none() && zsh_init("fish").is_none());
        if let Ok(mut child) = std::process::Command::new("zsh")
            .arg("-n")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            child.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
            assert!(
                child.wait().unwrap().success(),
                "zsh -n rejected the snippet"
            );
        }
    }
}
