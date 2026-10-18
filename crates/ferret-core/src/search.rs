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

/// Parallel search over the whole index; returns the best `limit` hits.
pub fn search(index: &Index, q: &Query, limit: usize, sort: Sort) -> SearchResult {
    todo!("W1 query agent")
}
