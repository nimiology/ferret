use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use memchr::memmem::Finder;
use rayon::prelude::*;

use crate::index::{FLAG_DELETED, NO_PARENT};
use crate::query::Cmp;
use crate::{Index, Kind, Query};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Score,
    Size,
    Mtime,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub node: u32,
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime: i64,
    pub score: i32,
}

#[derive(Clone, Debug, Default)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    pub total: usize,
    pub took_us: u64,
}

const CHUNK: usize = 1 << 16;
const NOISE: [&str; 6] = ["Caches", "node_modules", ".git", "DerivedData", ".Trash", "Library"];

/// (primary sort value, score, lower node id wins ties)
type Key = (i64, i32, Reverse<u32>);

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Node ids of directories whose full path equals `under`. None = no restriction.
fn under_ids(index: &Index, under: &str) -> Option<Vec<u32>> {
    if under == "/" {
        return None;
    }
    let last = under.rsplit('/').next().unwrap_or(under);
    Some(
        (0..index.len() as u32)
            .into_par_iter()
            .filter(|&i| {
                index.nodes[i as usize].kind == Kind::Dir && index.is_live(i) && index.name(i) == last && index.path(i) == under
            })
            .collect(),
    )
}

fn is_under(index: &Index, mut i: u32, roots: &[u32]) -> bool {
    loop {
        i = index.nodes[i as usize].parent;
        if i == NO_PARENT {
            return false;
        }
        if roots.contains(&i) {
            return true;
        }
    }
}

fn ext_matches(name_lower: &[u8], exts: &[String]) -> bool {
    let Some(dot) = memchr::memrchr(b'.', name_lower) else { return false };
    let ext = &name_lower[dot + 1..];
    exts.iter().any(|e| e.as_bytes() == ext)
}

/// Exact 1000 > prefix 600 > word start 400 > substring 200, per word.
fn word_score(name: &[u8], lower: &[u8], word: &[u8], pos: usize) -> i32 {
    if lower == word {
        return 1000;
    }
    if pos == 0 {
        return 600;
    }
    let prev = lower[pos - 1];
    // camelCase needs the original bytes, only comparable when lowercasing kept the length.
    let camel = name.len() == lower.len() && name[pos].is_ascii_uppercase() && name[pos - 1].is_ascii_lowercase();
    if matches!(prev, b' ' | b'-' | b'_' | b'.') || camel {
        400
    } else {
        200
    }
}

/// Depth penalty, noise penalty. Only called for matches.
fn place_penalty(index: &Index, i: u32) -> i32 {
    let mut depth = 0;
    let mut noisy = false;
    let mut p = index.nodes[i as usize].parent;
    while p != NO_PARENT {
        depth += 1;
        noisy = noisy || NOISE.contains(&index.name(p));
        p = index.nodes[p as usize].parent;
    }
    depth * 8 + if noisy { 400 } else { 0 }
}

/// Parallel search over the whole index; returns the best `limit` hits.
pub fn search(index: &Index, q: &Query, limit: usize, sort: Sort) -> SearchResult {
    let start = Instant::now();
    if q.is_empty() {
        return SearchResult::default();
    }
    let finders: Vec<Finder> = q.words.iter().map(|w| Finder::new(w.as_bytes())).collect();
    let not_finders: Vec<Finder> = q.not_words.iter().map(|w| Finder::new(w.as_bytes())).collect();
    let roots = q.under.as_deref().and_then(|u| under_ids(index, u));
    let now = now_secs();
    let phrase = q.words.join(" ");

    let n = index.len();
    let per_chunk: Vec<(usize, Vec<(Key, u32)>)> = (0..n.div_ceil(CHUNK))
        .into_par_iter()
        .map(|c| {
            let mut count = 0;
            let mut heap: BinaryHeap<Reverse<(Key, u32)>> = BinaryHeap::with_capacity(limit + 1);
            for i in (c * CHUNK).max(1)..((c + 1) * CHUNK).min(n) {
                let i = i as u32;
                let node = &index.nodes[i as usize];
                if node.flags & FLAG_DELETED != 0 || q.kind.is_some_and(|k| k != node.kind) {
                    continue;
                }
                if let Some((cmp, v)) = q.size {
                    let ok = match cmp {
                        Cmp::Lt => node.size < v,
                        Cmp::Gt => node.size > v,
                        Cmp::Eq => node.size == v,
                    };
                    if !ok {
                        continue;
                    }
                }
                if let Some((cmp, secs)) = q.modified_within {
                    let newer = node.mtime >= now - secs;
                    if newer == (cmp == Cmp::Gt) {
                        continue;
                    }
                }
                let lower = index.name_lower(i);
                if !q.exts.is_empty() && !ext_matches(lower, &q.exts) {
                    continue;
                }
                let mut score = 0;
                let mut ok = true;
                let name = index.name(i).as_bytes();
                for (f, w) in finders.iter().zip(&q.words) {
                    match f.find(lower) {
                        Some(pos) => score += word_score(name, lower, w.as_bytes(), pos),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok || not_finders.iter().any(|f| f.find(lower).is_some()) {
                    continue;
                }
                if let Some(r) = &roots {
                    if !is_under(index, i, r) {
                        continue;
                    }
                }
                count += 1;
                if finders.len() > 1 && lower == phrase.as_bytes() {
                    score += 1000;
                }
                if now - node.mtime < 7 * 86_400 {
                    score += 100;
                }
                score -= lower.len().min(100) as i32;
                let primary = match sort {
                    Sort::Score => 0,
                    Sort::Size => node.size.min(i64::MAX as u64) as i64,
                    Sort::Mtime => node.mtime,
                };
                // The place penalty only lowers the score: skip the parent walk if we can't enter the heap anyway.
                if heap.len() >= limit && heap.peek().is_none_or(|Reverse((min, _))| (primary, score, Reverse(i)) <= *min) {
                    continue;
                }
                score -= place_penalty(index, i);
                heap.push(Reverse(((primary, score, Reverse(i)), i)));
                if heap.len() > limit {
                    heap.pop();
                }
            }
            (count, heap.into_iter().map(|Reverse(x)| x).collect())
        })
        .collect();

    let total = per_chunk.iter().map(|(c, _)| c).sum();
    let mut best: Vec<(Key, u32)> = per_chunk.into_iter().flat_map(|(_, v)| v).collect();
    best.sort_unstable_by_key(|b| Reverse(b.0));
    best.truncate(limit);
    let hits = best
        .into_iter()
        .map(|((_, score, _), i)| {
            let node = &index.nodes[i as usize];
            Hit { node: i, path: index.path(i), kind: node.kind, size: node.size, mtime: node.mtime, score }
        })
        .collect();
    SearchResult { hits, total, took_us: start.elapsed().as_micros() as u64 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn idx() -> Index {
        let now = now_secs();
        let old = now - 400 * 86_400;
        let mut ix = Index::default();
        let root = ix.push(NO_PARENT, "/", Kind::Dir, 0, old);
        let users = ix.push(root, "Users", Kind::Dir, 0, old);
        let me = ix.push(users, "me", Kind::Dir, 0, old);
        let docs = ix.push(me, "Documents", Kind::Dir, 0, old);
        ix.push(docs, "invoice.pdf", Kind::File, 2 << 20, now - 86_400);
        ix.push(docs, "old_invoice_2025.pdf", Kind::File, 10, old);
        ix.push(docs, "invoice-draft.pdf", Kind::File, 500, old);
        ix.push(docs, "MyInvoice.txt", Kind::File, 1 << 30, old);
        ix.push(docs, "photo.JPG", Kind::File, 100, old);
        let lib = ix.push(me, "Library", Kind::Dir, 0, old);
        let caches = ix.push(lib, "Caches", Kind::Dir, 0, old);
        ix.push(caches, "invoice", Kind::File, 1, old);
        ix.push(me, "invoices", Kind::Dir, 0, old);
        ix
    }

    fn names(r: &SearchResult) -> Vec<String> {
        r.hits.iter().map(|h| h.path.rsplit('/').next().unwrap().to_string()).collect()
    }

    fn run(q: &str) -> SearchResult {
        search(&idx(), &parse(q).unwrap(), 100, Sort::Score)
    }

    #[test]
    fn empty_query() {
        let r = run("");
        assert_eq!((r.total, r.hits.len()), (0, 0));
    }

    #[test]
    fn substring_and_ranking() {
        let r = run("invoice");
        assert_eq!(r.total, 6);
        let n = names(&r);
        let at = |s: &str| n.iter().position(|x| x == s).unwrap();
        assert_eq!(n[0], "invoice.pdf"); // prefix + recent
        // Exact name under Library/Caches is pushed below a clean prefix match.
        assert!(at("invoices") < at("invoice"));
        assert!(at("MyInvoice.txt") < at("old_invoice_2025.pdf")); // camel start beats substring
    }

    #[test]
    fn filters() {
        assert_eq!(names(&run("invoice *.pdf !draft")), ["invoice.pdf", "old_invoice_2025.pdf"]);
        assert_eq!(names(&run("ext:jpg")), ["photo.JPG"]);
        assert_eq!(names(&run("invoice type:dir")), ["invoices"]);
        assert_eq!(names(&run("invoice size:>1MB")).len(), 2);
        assert_eq!(names(&run("invoice mod:<7d")), ["invoice.pdf"]);
        assert_eq!(run("invoice mod:>7d").total, 5);
        assert_eq!(names(&run("old 2025")), ["old_invoice_2025.pdf"]);
    }

    #[test]
    fn under_filter() {
        assert_eq!(run("invoice in:/Users/me/Library").total, 1);
        assert_eq!(run("invoice in:/Users/me/").total, 6);
        assert_eq!(run("invoice in:/nope").total, 0);
        assert_eq!(run("in:/Users/me/Library").total, 2);
    }

    #[test]
    fn root_never_returned_and_limit() {
        let r = search(&idx(), &parse("type:dir").unwrap(), 2, Sort::Score);
        assert_eq!(r.total, 6);
        assert_eq!(r.hits.len(), 2);
        assert!(r.hits.iter().all(|h| h.node != 0));
    }

    #[test]
    fn sort_by_size() {
        let r = search(&idx(), &parse("invoice type:file").unwrap(), 2, Sort::Size);
        assert_eq!(names(&r), ["MyInvoice.txt", "invoice.pdf"]);
    }
}
