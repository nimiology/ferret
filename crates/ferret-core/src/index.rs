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
/// Tombstone: the entry is gone from disk. Search skips it; `compact` drops it.
pub const FLAG_DELETED: u8 = 2;

#[derive(Default, Serialize, Deserialize)]
pub struct Index {
    pub nodes: Vec<Node>,
    /// Every name, UTF-8, NFC-normalized, back to back.
    pub names: Vec<u8>,
    /// Same bytes as `names`, lowercased (ASCII + simple Unicode), same offsets when lengths match.
    pub names_lower: Vec<u8>,
    /// Offsets into `names_lower` (lowercasing can change byte length): (off, len) per node.
    pub lower_spans: Vec<(u32, u16)>,
    /// Child links (NO_PARENT = none). Derived from `parent`, rebuilt by `relink` after load.
    #[serde(skip)]
    pub first_child: Vec<u32>,
    #[serde(skip)]
    pub next_sibling: Vec<u32>,
    /// Number of tombstoned nodes.
    #[serde(skip)]
    pub deleted: usize,
}

impl Index {
    /// Rebuild child links and the tombstone count from `nodes`.
    pub fn relink(&mut self) {
        let n = self.nodes.len();
        self.first_child = vec![NO_PARENT; n];
        self.next_sibling = vec![NO_PARENT; n];
        self.deleted = 0;
        for i in (0..n).rev() {
            let node = self.nodes[i];
            if node.flags & FLAG_DELETED != 0 {
                self.deleted += 1;
            }
            if node.parent != NO_PARENT {
                self.next_sibling[i] = self.first_child[node.parent as usize];
                self.first_child[node.parent as usize] = i as u32;
            }
        }
    }

    pub fn is_live(&self, i: u32) -> bool {
        self.nodes[i as usize].flags & FLAG_DELETED == 0
    }

    pub fn live_count(&self) -> usize {
        self.nodes.len() - self.deleted
    }

    /// Live direct children of `dir`.
    pub fn children(&self, dir: u32) -> impl Iterator<Item = u32> + '_ {
        let mut c = self.first_child.get(dir as usize).copied().unwrap_or(NO_PARENT);
        std::iter::from_fn(move || {
            while c != NO_PARENT {
                let i = c;
                c = self.next_sibling[i as usize];
                if self.is_live(i) {
                    return Some(i);
                }
            }
            None
        })
    }

    /// Node for an absolute path (NFC components), walking from the root by name.
    pub fn find(&self, path: &std::path::Path) -> Option<u32> {
        use unicode_normalization::UnicodeNormalization;
        if self.is_empty() {
            return None;
        }
        let rel = path.strip_prefix(self.name(0)).ok()?;
        let mut cur = 0u32;
        for comp in rel.components() {
            let std::path::Component::Normal(c) = comp else { return None };
            let c: String = c.to_string_lossy().nfc().collect();
            cur = self.children(cur).find(|&i| self.name(i) == c)?;
        }
        Some(cur)
    }

    /// Tombstone `i` and all its live descendants.
    pub fn remove(&mut self, i: u32) {
        let mut stack = vec![i];
        while let Some(i) = stack.pop() {
            if !self.is_live(i) {
                continue;
            }
            stack.extend(self.children(i));
            self.nodes[i as usize].flags |= FLAG_DELETED;
            self.deleted += 1;
        }
    }

    /// Rebuild all arrays without tombstones. Node ids change; the root stays 0.
    pub fn compact(&mut self) {
        let mut out = Index::default();
        let mut remap = vec![NO_PARENT; self.len()];
        for i in 0..self.len() {
            let n = self.nodes[i];
            if n.flags & FLAG_DELETED != 0 {
                continue;
            }
            // Parents always have lower ids than children (append-only), so remap is already filled.
            let parent = if n.parent == NO_PARENT { NO_PARENT } else { remap[n.parent as usize] };
            let id = out.push(parent, self.name(i as u32), n.kind, n.size, n.mtime);
            out.nodes[id as usize].flags = n.flags;
            remap[i] = id;
        }
        *self = out;
    }

    /// Compact when more than 20% of nodes are tombstones.
    pub fn maybe_compact(&mut self) -> bool {
        let due = self.deleted * 5 > self.len();
        if due {
            self.compact();
        }
        due
    }

    /// Append a node. `name` must already be NFC-normalized.
    /// The root node (parent == NO_PARENT) should be named with its absolute path, e.g. "/".
    pub fn push(&mut self, parent: u32, name: &str, kind: Kind, size: u64, mtime: i64) -> u32 {
        if self.first_child.len() != self.nodes.len() {
            self.relink();
        }
        let name = truncate(name);
        let lower = name.to_lowercase();
        let lower = truncate(&lower);
        let id = self.nodes.len() as u32;
        self.first_child.push(NO_PARENT);
        self.next_sibling.push(NO_PARENT);
        if parent != NO_PARENT {
            self.next_sibling[id as usize] = self.first_child[parent as usize];
            self.first_child[parent as usize] = id;
        }
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
