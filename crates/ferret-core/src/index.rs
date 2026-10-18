use serde::{Deserialize, Serialize};

pub const NO_PARENT: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
#[serde(rename_all = "lowercase")]
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
    /// The root node (parent == NO_PARENT) should be named with its absolute path, e.g. "/".
    pub fn push(&mut self, parent: u32, name: &str, kind: Kind, size: u64, mtime: i64) -> u32 {
        let name = truncate(name);
        let lower = name.to_lowercase();
        let lower = truncate(&lower);
        let id = self.nodes.len() as u32;
        self.nodes.push(Node {
            parent,
            name_off: self.names.len() as u32,
            name_len: name.len() as u16,
            kind,
            flags: if name.starts_with('.') { FLAG_HIDDEN } else { 0 },
            size,
            mtime,
        });
        self.names.extend_from_slice(name.as_bytes());
        self.lower_spans.push((self.names_lower.len() as u32, lower.len() as u16));
        self.names_lower.extend_from_slice(lower.as_bytes());
        id
    }

    pub fn name(&self, i: u32) -> &str {
        let n = &self.nodes[i as usize];
        let off = n.name_off as usize;
        // Only valid UTF-8 is ever pushed.
        std::str::from_utf8(&self.names[off..off + n.name_len as usize]).unwrap_or("")
    }

    pub fn name_lower(&self, i: u32) -> &[u8] {
        let (off, len) = self.lower_spans[i as usize];
        &self.names_lower[off as usize..off as usize + len as usize]
    }

    /// Rebuild the full path by walking parent links.
    pub fn path(&self, i: u32) -> String {
        let mut chain = Vec::new();
        let mut cur = i;
        while cur != NO_PARENT {
            chain.push(cur);
            cur = self.nodes[cur as usize].parent;
        }
        let mut out = String::new();
        for &id in chain.iter().rev() {
            if !out.is_empty() && !out.ends_with('/') {
                out.push('/');
            }
            out.push_str(self.name(id));
        }
        out
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// Clamp to u16::MAX bytes at a char boundary.
fn truncate(s: &str) -> &str {
    let mut end = s.len().min(u16::MAX as usize);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_rebuild() {
        let mut ix = Index::default();
        let r = ix.push(NO_PARENT, "/", Kind::Dir, 0, 0);
        let u = ix.push(r, "Users", Kind::Dir, 0, 0);
        let x = ix.push(u, "x", Kind::Dir, 0, 0);
        let f = ix.push(x, "a.pdf", Kind::File, 3, 0);
        assert_eq!(ix.path(r), "/");
        assert_eq!(ix.path(f), "/Users/x/a.pdf");
        let mut ix2 = Index::default();
        let r = ix2.push(NO_PARENT, "/tmp/root", Kind::Dir, 0, 0);
        let f = ix2.push(r, ".env", Kind::File, 0, 0);
        assert_eq!(ix2.path(f), "/tmp/root/.env");
        assert_eq!(ix2.nodes[f as usize].flags & FLAG_HIDDEN, FLAG_HIDDEN);
    }

    #[test]
    fn lower_spans() {
        let mut ix = Index::default();
        let a = ix.push(NO_PARENT, "/", Kind::Dir, 0, 0);
        let b = ix.push(a, "Résumé.PDF", Kind::File, 0, 0);
        let c = ix.push(a, "نامه‌ها", Kind::File, 0, 0);
        let d = ix.push(a, "İX", Kind::File, 0, 0); // lowercase changes byte length
        let e = ix.push(a, "ABC", Kind::File, 0, 0);
        assert_eq!(ix.name(b), "Résumé.PDF");
        assert_eq!(ix.name_lower(b), "résumé.pdf".as_bytes());
        assert_eq!(ix.name_lower(c), "نامه‌ها".as_bytes());
        assert_eq!(ix.name_lower(d), "i̇x".as_bytes());
        assert_eq!(ix.name_lower(e), b"abc");
        assert_eq!(ix.name(e), "ABC");
    }

    #[test]
    fn long_name_truncates_on_char_boundary() {
        let mut ix = Index::default();
        let long = "é".repeat(40_000); // 80_000 bytes
        let i = ix.push(NO_PARENT, &long, Kind::File, 0, 0);
        assert!(ix.name(i).len() <= u16::MAX as usize);
        assert!(ix.name(i).chars().all(|c| c == 'é'));
    }
}
