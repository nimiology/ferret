//! Ferret core: the index, scanner, query parser, search engine and snapshot.

pub mod exclude;
pub mod index;
pub mod protocol;
pub mod query;
pub mod scan;
pub mod search;
pub mod snapshot;

pub use exclude::Excludes;
pub use index::{Index, Kind, Node};
pub use query::{parse, Query};
pub use search::{search, Hit, SearchResult, Sort};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("snapshot: {0}")]
    Snapshot(#[from] bincode::Error),
    #[error("bad query: {0}")]
    Query(String),
    #[error("config: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, Error>;
