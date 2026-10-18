use crate::{Error, Kind, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Gt,
    Eq,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    /// Lowercased words; a name must contain all of them.
    pub words: Vec<String>,
    /// Lowercased words a name must NOT contain (`!word`).
    pub not_words: Vec<String>,
    /// Lowercased extensions without the dot (`*.pdf`, `ext:pdf`, `ext:pdf;jpg`). Any of them.
    pub exts: Vec<String>,
    pub kind: Option<Kind>,
    /// `size:>100MB` -> (Gt, 104857600)
    pub size: Option<(Cmp, u64)>,
    /// `mod:<7d` -> (Lt, seconds). Lt means "newer than", Gt "older than".
    pub modified_within: Option<(Cmp, i64)>,
    /// `in:~/Projects`, tilde expanded, no trailing slash.
    pub under: Option<String>,
}

impl Query {
    /// True when nothing would restrict the search.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
            && self.not_words.is_empty()
            && self.exts.is_empty()
            && self.kind.is_none()
            && self.size.is_none()
            && self.modified_within.is_none()
            && self.under.is_none()
    }
}

fn err(msg: impl Into<String>) -> Error {
    Error::Query(msg.into())
}

/// Whitespace split; `"a b"` is one token. Returns (text, was_quoted).
fn tokenize(input: &str) -> Result<Vec<(String, bool)>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut in_quote = false;
    for c in input.chars() {
        if c == '"' {
            in_quote = !in_quote;
            quoted = true;
        } else if c.is_whitespace() && !in_quote {
            if !cur.is_empty() {
                out.push((std::mem::take(&mut cur), quoted));
            }
            quoted = false;
        } else {
            cur.push(c);
        }
    }
    if in_quote {
        return Err(err("unclosed double quote"));
    }
    if !cur.is_empty() {
        out.push((cur, quoted));
    }
    Ok(out)
}

fn split_cmp(s: &str) -> (Cmp, &str) {
    match s.as_bytes().first() {
        Some(b'<') => (Cmp::Lt, &s[1..]),
        Some(b'>') => (Cmp::Gt, &s[1..]),
        Some(b'=') => (Cmp::Eq, &s[1..]),
        _ => (Cmp::Eq, s),
    }
}

/// "1.5" + "gb" -> (1.5, "gb")
fn split_num(s: &str) -> Option<(f64, String)> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let n: f64 = s[..end].parse().ok()?;
    Some((n, s[end..].to_ascii_lowercase()))
}

fn parse_size(v: &str) -> Result<(Cmp, u64)> {
    let bad = || {
        err(format!(
            "bad size '{v}': use e.g. size:>100MB, size:<1.5GB (units B, KB, MB, GB, TB)"
        ))
    };
    let (cmp, rest) = split_cmp(v);
    let (n, unit) = split_num(rest).ok_or_else(bad)?;
    let mult: u64 = match unit.as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        "t" | "tb" => 1 << 40,
        _ => return Err(bad()),
    };
    Ok((cmp, (n * mult as f64) as u64))
}

fn parse_age(v: &str) -> Result<(Cmp, i64)> {
    let bad = || {
        err(format!(
            "bad age '{v}': use e.g. mod:<7d, mod:>1y (units s, m, h, d, w, y)"
        ))
    };
    let (cmp, rest) = split_cmp(v);
    // No operator on an age means "within".
    let cmp = if v.starts_with('=') || v.starts_with(['<', '>']) {
        cmp
    } else {
        Cmp::Lt
    };
    let (n, unit) = split_num(rest).ok_or_else(bad)?;
    let mult: i64 = match unit.as_str() {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        "y" => 365 * 86_400,
        _ => return Err(bad()),
    };
    Ok((cmp, (n * mult as f64) as i64))
}

fn expand_dir(v: &str) -> Result<String> {
    if v.is_empty() {
        return Err(err("in: needs a folder, e.g. in:~/Projects"));
    }
    let mut p = if v == "~" || v.starts_with("~/") {
        let home = std::env::var("HOME").map_err(|_| err("in: uses ~ but $HOME is not set"))?;
        format!("{home}{}", &v[1..])
    } else {
        v.to_string()
    };
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    // Resolve `.` and `..` lexically so in:~/a/../b matches the indexed path.
    if p.starts_with('/') {
        let mut parts: Vec<&str> = Vec::new();
        for c in p.split('/') {
            match c {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                c => parts.push(c),
            }
        }
        p = format!("/{}", parts.join("/"));
    }
    Ok(p)
}

pub fn parse(input: &str) -> Result<Query> {
    use unicode_normalization::UnicodeNormalization;
    // The index stores NFC; text pasted from macOS is often NFD.
    let input: String = input.nfc().collect();
    let mut q = Query::default();
    for (tok, quoted) in tokenize(&input)? {
        if quoted {
            // Quotes protect spaces, not the `!` / `in:` prefix: !"my draft", in:"~/My Folder".
            if let Some(w) = tok.strip_prefix('!').filter(|w| !w.is_empty()) {
                q.not_words.push(w.to_lowercase());
            } else if let Some(d) = tok.strip_prefix("in:").filter(|d| !d.is_empty()) {
                q.under = Some(expand_dir(d)?);
            } else {
                q.words.push(tok.to_lowercase());
            }
            continue;
        }
        if let Some(w) = tok.strip_prefix('!') {
            if w.is_empty() {
                return Err(err("'!' needs a word after it, e.g. !draft"));
            }
            q.not_words.push(w.to_lowercase());
            continue;
        }
        if let Some(e) = tok.strip_prefix("*.") {
            if e.is_empty() {
                return Err(err("'*.' needs an extension, e.g. *.pdf"));
            }
            q.exts.push(e.to_lowercase());
            continue;
        }
        let Some((key, val)) = tok.split_once(':') else {
            q.words.push(tok.to_lowercase());
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            "ext" => {
                let before = q.exts.len();
                q.exts.extend(
                    val.split([';', ','])
                        .map(|e| e.trim_start_matches('.').to_lowercase())
                        .filter(|e| !e.is_empty()),
                );
                if q.exts.len() == before {
                    return Err(err("ext: needs an extension, e.g. ext:pdf or ext:pdf;jpg"));
                }
            }
            "type" => {
                q.kind = Some(match val.to_ascii_lowercase().as_str() {
                    "file" | "f" => Kind::File,
                    "dir" | "folder" | "d" => Kind::Dir,
                    _ => return Err(err(format!("bad type '{val}': use type:file or type:dir"))),
                })
            }
            "size" => q.size = Some(parse_size(val)?),
            "mod" => q.modified_within = Some(parse_age(val)?),
            "in" => q.under = Some(expand_dir(val)?),
            _ => q.words.push(tok.to_lowercase()),
        }
    }
    Ok(q)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_lowercased() {
        let q = parse("Invoice  2025").unwrap();
        assert_eq!(q.words, ["invoice", "2025"]);
        assert!(parse("   ").unwrap().is_empty());
    }

    #[test]
    fn quoted_phrase() {
        let q = parse(r#""My Report" x"#).unwrap();
        assert_eq!(q.words, ["my report", "x"]);
        // Quoted tokens are never filters.
        assert_eq!(parse(r#""ext:pdf""#).unwrap().words, ["ext:pdf"]);
        assert!(parse(r#""open"#).is_err());
    }

    #[test]
    fn not_word() {
        let q = parse("a !Draft").unwrap();
        assert_eq!(q.not_words, ["draft"]);
        assert!(parse("!").is_err());
    }

    #[test]
    fn exts() {
        assert_eq!(parse("*.PDF").unwrap().exts, ["pdf"]);
        assert_eq!(parse("ext:pdf;JPG").unwrap().exts, ["pdf", "jpg"]);
        assert_eq!(parse("*.md ext:.txt").unwrap().exts, ["md", "txt"]);
        assert!(parse("ext:").is_err());
        assert!(parse("*.").is_err());
    }

    #[test]
    fn kind() {
        assert_eq!(parse("type:file").unwrap().kind, Some(Kind::File));
        assert_eq!(parse("type:Folder").unwrap().kind, Some(Kind::Dir));
        assert_eq!(parse("type:dir").unwrap().kind, Some(Kind::Dir));
        assert!(parse("type:blob").is_err());
    }

    #[test]
    fn size() {
        assert_eq!(
            parse("size:>100MB").unwrap().size,
            Some((Cmp::Gt, 100 << 20))
        );
        assert_eq!(parse("size:<1kb").unwrap().size, Some((Cmp::Lt, 1024)));
        assert_eq!(parse("size:=10").unwrap().size, Some((Cmp::Eq, 10)));
        assert_eq!(parse("size:>1.5GB").unwrap().size, Some((Cmp::Gt, 3 << 29)));
        assert_eq!(parse("size:>2tb").unwrap().size, Some((Cmp::Gt, 2 << 40)));
        assert!(parse("size:>lots").is_err());
        assert!(parse("size:>5XB").is_err());
    }

    #[test]
    fn modified() {
        assert_eq!(
            parse("mod:<7d").unwrap().modified_within,
            Some((Cmp::Lt, 7 * 86_400))
        );
        assert_eq!(
            parse("mod:>1y").unwrap().modified_within,
            Some((Cmp::Gt, 365 * 86_400))
        );
        assert_eq!(
            parse("mod:2h").unwrap().modified_within,
            Some((Cmp::Lt, 7200))
        );
        assert_eq!(
            parse("mod:<30m").unwrap().modified_within,
            Some((Cmp::Lt, 1800))
        );
        assert_eq!(
            parse("mod:<1w").unwrap().modified_within,
            Some((Cmp::Lt, 604_800))
        );
        assert!(parse("mod:<7").is_err());
    }

    #[test]
    fn under() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            parse("in:~/Projects/").unwrap().under,
            Some(format!("{home}/Projects"))
        );
        assert_eq!(parse("in:/tmp").unwrap().under.as_deref(), Some("/tmp"));
        assert_eq!(parse("in:/").unwrap().under.as_deref(), Some("/"));
        assert!(parse("in:").is_err());
    }

    #[test]
    fn unknown_key_is_word() {
        assert_eq!(parse("http://x").unwrap().words, ["http://x"]);
        assert_eq!(parse("Foo:Bar").unwrap().words, ["foo:bar"]);
    }

    #[test]
    fn combo() {
        let q = parse(r#"invoice "Q1 2025" *.pdf !draft size:>1MB mod:<30d in:/Users type:file"#)
            .unwrap();
        assert_eq!(q.words, ["invoice", "q1 2025"]);
        assert_eq!(q.not_words, ["draft"]);
        assert_eq!(q.exts, ["pdf"]);
        assert_eq!(q.kind, Some(Kind::File));
        assert_eq!(q.size, Some((Cmp::Gt, 1 << 20)));
        assert_eq!(q.modified_within, Some((Cmp::Lt, 30 * 86_400)));
        assert_eq!(q.under.as_deref(), Some("/Users"));
    }

    #[test]
    fn bughunt_regressions() {
        // quoted `!` excludes; quoted in: keeps spaces
        let q = parse(r#"txt !"my draft" in:"/tmp/My Dir/""#).unwrap();
        assert_eq!(q.words, ["txt"]);
        assert_eq!(q.not_words, ["my draft"]);
        assert_eq!(q.under.as_deref(), Some("/tmp/My Dir"));
        // NFD input is normalized to NFC like the index
        assert_eq!(parse("cafe\u{301}").unwrap().words, ["caf\u{e9}"]);
        // `.` and `..` resolved
        assert_eq!(
            parse("in:/a/b/../c/./d").unwrap().under.as_deref(),
            Some("/a/c/d")
        );
    }
}
