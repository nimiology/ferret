use serde::{Deserialize, Serialize};

pub const NO_PARENT: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Kind {
    File = 0,
    Dir = 1,
    Symlink = 2,
}

/// One file or folder. Nodes point at each other by index into `Index::nodes`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[repr(C)]
pub struct Node {
    pub parent: u32,
    pub name_off: u32,
    pub name_len: u16,
    pub kind: Kind,
    pub flags: u8,
    pub size: u64,
    pub mtime: i64,
}

pub const FLAG_HIDDEN: u8 = 1;

#[derive(Default, Serialize, Deserialize)]
pub struct Index {
    pub nodes: Vec<Node>,
    /// Every name, UTF-8, NFC-normalized, back to back.
    pub names: Vec<u8>,
    /// Same bytes as `names`, lowercased (ASCII + simple Unicode), same offsets when lengths match.
    pub names_lower: Vec<u8>,
    /// Offsets into `names_lower` (lowercasing can change byte length): (off, len) per node.
    pub lower_spans: Vec<(u32, u16)>,
}

impl Index {
    /// Append a node. `name` must already be NFC-normalized.
    pub fn push(&mut self, parent: u32, name: &str, kind: Kind, size: u64, mtime: i64) -> u32 {
        todo!("W1 scanner agent")
    }

    pub fn name(&self, i: u32) -> &str {
        todo!("W1 scanner agent")
    }

    pub fn name_lower(&self, i: u32) -> &[u8] {
        todo!("W1 scanner agent")
    }

    /// Rebuild the full path by walking parent links.
    pub fn path(&self, i: u32) -> String {
        todo!("W1 scanner agent")
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}
