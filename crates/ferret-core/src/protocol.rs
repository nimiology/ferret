//! Newline-delimited JSON over a Unix socket. Every front end speaks this.

use crate::{Kind, Sort};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

pub const VERSION: u32 = 1;

/// `~/Library/Application Support/ferret/ferret.sock` (0600).
pub fn socket_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join("Library/Application Support/ferret/ferret.sock")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KindFilter {
    #[default]
    All,
    File,
    Dir,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Op {
    Search {
        q: String,
        #[serde(default = "default_limit")]
        limit: usize,
        #[serde(default)]
        sort: SortArg,
        #[serde(default)]
        kind: KindFilter,
    },
    Status,
}

fn default_limit() -> usize {
    50
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortArg {
    #[default]
    Score,
    Size,
    Mtime,
}

impl From<SortArg> for Sort {
    fn from(s: SortArg) -> Sort {
        match s {
            SortArg::Score => Sort::Score,
            SortArg::Size => Sort::Size,
            SortArg::Mtime => Sort::Mtime,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub id: u64,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HitOut {
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    /// Live (non-tombstoned) entries.
    pub files: u64,
    pub indexing: bool,
    /// 0.0..=1.0 while indexing, best guess.
    pub progress: f32,
    pub current_path: Option<String>,
    pub last_event_id: u64,
    pub permission_denied: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub took_us: u64,
    #[serde(default)]
    pub total: usize,
    /// Matches per kind (file, dir, symlink) ignoring the kind filter, for tab counts.
    #[serde(default)]
    pub by_kind: [usize; 3],
    #[serde(default)]
    pub hits: Vec<HitOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

/// One blocking request/response round trip. Err = daemon not reachable or bad reply.
pub fn request(op: Op) -> std::io::Result<Response> {
    let mut s = UnixStream::connect(socket_path())?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = serde_json::to_vec(&Request {
        v: VERSION,
        id: 1,
        op,
    })?;
    line.push(b'\n');
    s.write_all(&line)?;
    let mut buf = String::new();
    BufReader::new(s).read_line(&mut buf)?;
    Ok(serde_json::from_str(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_matches_plan() {
        let r: Request = serde_json::from_str(
            r#"{"v":1,"id":7,"op":"search","q":"invoice *.pdf","limit":50,"sort":"score"}"#,
        )
        .unwrap();
        assert!(matches!(r.op, Op::Search { ref q, limit: 50, .. } if q == "invoice *.pdf"));
        let r: Request = serde_json::from_str(r#"{"v":1,"id":8,"op":"status"}"#).unwrap();
        assert!(matches!(r.op, Op::Status));
    }
}
