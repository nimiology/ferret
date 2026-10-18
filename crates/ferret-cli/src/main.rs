use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use ferret_core::{snapshot, Node, Sort};

const ACCENT: &str = "\x1b[1;38;5;208m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Build the file index.
#[derive(Parser)]
#[command(name = "ff index")]
struct IndexArgs {
    #[arg(long, default_value = "/")]
    root: PathBuf,
    #[arg(long)]
    stats: bool,
}

/// Instant file search. `ff index` builds the index; anything else is a query.
#[derive(Parser)]
#[command(name = "ff", version)]
struct QueryArgs {
    #[arg(required = true)]
    query: Vec<String>,
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[arg(long, value_enum, default_value_t = SortArg::Score)]
    sort: SortArg,
}

#[derive(Clone, Copy, ValueEnum)]
enum SortArg {
    Score,
    Size,
    Mtime,
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    // `index` is the only subcommand; everything else is a query.
    if argv.get(1).map(String::as_str) == Some("index") {
        run_index(IndexArgs::parse_from(&argv[1..]))
    } else {
        run_query(QueryArgs::parse_from(&argv))
    }
}

fn run_index(a: IndexArgs) -> Result<()> {
    let (index, st) = ferret_core::scan::scan(&a.root)
        .with_context(|| format!("could not scan {}", a.root.display()))?;
    let path = snapshot::default_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    }
    snapshot::save(&index, &path).with_context(|| format!("could not save snapshot to {}", path.display()))?;
    let disk = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    println!("indexed {} files in {:.2?}", fmt_count(st.files), st.elapsed);
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
        println!("  snapshot           {} ({})", fmt_bytes(disk), path.display());
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

fn run_query(a: QueryArgs) -> Result<()> {
    let path = snapshot::default_path();
    if !path.exists() {
        eprintln!("no index yet — run `ff index` first");
        std::process::exit(1);
    }
    let index = snapshot::load(&path)
        .with_context(|| format!("could not read index at {} (try `ff index` again)", path.display()))?;
    let q = match ferret_core::parse(&a.query.join(" ")) {
        Ok(q) => q,
        Err(e) => bail!("{e}"),
    };
    let sort = match a.sort {
        SortArg::Score => Sort::Score,
        SortArg::Size => Sort::Size,
        SortArg::Mtime => Sort::Mtime,
    };
    let res = ferret_core::search(&index, &q, a.limit, sort);

    let tty = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let mut out = std::io::stdout().lock();
    for h in &res.hits {
        let line = if tty { highlight(&h.path, &q.words) } else { h.path.clone() };
        if writeln!(out, "{line}").is_err() {
            return Ok(()); // broken pipe (e.g. `| head`)
        }
    }
    if tty {
        eprintln!(
            "{DIM}{} matches · {} ms · {} files indexed{RESET}",
            res.total,
            res.took_us / 1000,
            fmt_count(index.nodes.len() as u64)
        );
    }
    Ok(())
}

/// Dim the parent folder, bold+accent each query word inside the file name.
fn highlight(path: &str, words: &[String]) -> String {
    let cut = path.trim_end_matches('/').rfind('/').map_or(0, |i| i + 1);
    let (parent, name) = path.split_at(cut);
    // ASCII lowercasing keeps byte offsets aligned with the original string.
    let lower = name.to_ascii_lowercase();
    let mut hit = vec![false; name.len()];
    for w in words.iter().filter(|w| !w.is_empty()) {
        let w = w.to_ascii_lowercase();
        let mut from = 0;
        while let Some(i) = lower[from..].find(&w) {
            hit[from + i..from + i + w.len()].iter_mut().for_each(|b| *b = true);
            from += i + w.len();
        }
    }
    let mut s = String::new();
    if !parent.is_empty() {
        s.push_str(&format!("{DIM}{parent}{RESET}"));
    }
    let mut i = 0;
    while i < name.len() {
        let on = hit[i];
        let j = (i..name.len()).find(|&k| hit[k] != on).unwrap_or(name.len());
        if on {
            s.push_str(&format!("{ACCENT}{}{RESET}", &name[i..j]));
        } else {
            s.push_str(&name[i..j]);
        }
        i = j;
    }
    s
}

/// 2_100_000 -> "2.1M", 840_000 -> "840K", 999 -> "999".
fn fmt_count(n: u64) -> String {
    let f = |v: f64, u: &str| {
        let s = if v < 10.0 { format!("{v:.1}") } else { format!("{v:.0}") };
        format!("{}{u}", s.trim_end_matches(".0"))
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => f(n as f64 / 1e3, "K"),
        _ => f(n as f64 / 1e6, "M"),
    }
}

fn fmt_bytes(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.1} KiB", n as f64 / 1024.0),
        _ => format!("{:.1} MiB", n as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts() {
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(840_000), "840K");
        assert_eq!(fmt_count(2_100_000), "2.1M");
        assert_eq!(fmt_count(1_000), "1K");
        assert_eq!(fmt_count(12_400_000), "12M");
    }

    #[test]
    fn highlights_name_only() {
        let s = highlight("/foo/Foo-bar.txt", &["foo".into()]);
        assert_eq!(s, format!("{DIM}/foo/{RESET}{ACCENT}Foo{RESET}-bar.txt"));
        assert_eq!(highlight("x", &[]), "x");
    }
}
