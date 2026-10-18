use crate::{Kind, Result};

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

pub fn parse(input: &str) -> Result<Query> {
    todo!("W1 query agent")
}
