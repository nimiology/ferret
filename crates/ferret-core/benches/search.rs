use criterion::{black_box, criterion_group, criterion_main, Criterion};
use ferret_core::index::NO_PARENT;
use ferret_core::{parse, search, Index, Kind, Sort};

const WORDS: [&str; 24] = [
    "report", "invoice", "photo", "Project", "notes", "backup", "draft", "final", "config", "main", "test",
    "data", "image", "budget", "meeting", "readme", "src", "build", "assets", "lib", "cache", "client", "server",
    "summary",
];
const EXTS: [&str; 10] = ["pdf", "txt", "jpg", "png", "rs", "js", "json", "md", "swift", "docx"];

/// xorshift, deterministic.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

/// 2M nodes, ~10% dirs, depth up to 8.
fn build(n: usize) -> Index {
    let mut r = Rng(0x9E3779B97F4A7C15);
    let mut ix = Index::default();
    let root = ix.push(NO_PARENT, "/", Kind::Dir, 0, 0);
    let mut dirs: Vec<(u32, u8)> = vec![(root, 0)];
    while ix.len() < n {
        let (parent, depth) = dirs[(r.next() % dirs.len() as u64) as usize];
        let name = format!("{}_{}{}", r.pick(&WORDS), r.pick(&WORDS), r.next() % 1000);
        let mtime = 1_600_000_000 + (r.next() % 200_000_000) as i64;
        if depth < 8 && r.next().is_multiple_of(10) {
            let id = ix.push(parent, &name, Kind::Dir, 0, mtime);
            dirs.push((id, depth + 1));
        } else {
            let name = format!("{name}.{}", r.pick(&EXTS));
            ix.push(parent, &name, Kind::File, r.next() % (50 << 20), mtime);
        }
    }
    ix
}

fn bench(c: &mut Criterion) {
    let ix = build(2_000_000);
    for q in ["invoi", "*.pdf", "project budget"] {
        let parsed = parse(q).unwrap();
        c.bench_function(&format!("search 2M '{q}'"), |b| {
            b.iter(|| black_box(search(&ix, &parsed, 100, Sort::Score)))
        });
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
