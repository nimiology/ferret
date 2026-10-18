use crate::{Index, Result};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"FERRET01";
/// v2: `Meta` header before the index.
const VERSION: u32 = 2;

/// Daemon bookkeeping stored with the index.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    /// Last FSEvents event id applied; 0 = unknown.
    pub last_event_id: u64,
    /// Root volume UUID when the snapshot was written; empty = unknown.
    pub volume_uuid: String,
}

/// `~/Library/Caches/ferret/index.bin`
pub fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join("Library/Caches/ferret/index.bin")
}

/// Write atomically (temp file + rename).
pub fn save(index: &Index, path: &Path) -> Result<()> {
    save_with(index, &Meta::default(), path)
}

/// `save` plus daemon metadata.
pub fn save_with(index: &Index, meta: &Meta, path: &Path) -> Result<()> {
    write_bytes(&encode(index, meta)?, path)
}

/// Serialize in memory, so a caller holding a lock can release it before the slow disk write.
pub fn encode(index: &Index, meta: &Meta) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(index.names.len() * 3 + index.nodes.len() * 40);
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
    bincode::serialize_into(&mut buf, meta)?;
    bincode::serialize_into(&mut buf, index)?;
    Ok(buf)
}

/// Write `encode` output atomically (temp file + rename).
pub fn write_bytes(bytes: &[u8], path: &Path) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let res = (|| {
        let mut w = BufWriter::new(File::create(&tmp)?);
        w.write_all(bytes)?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

pub fn load(path: &Path) -> Result<Index> {
    Ok(load_with(path)?.0)
}

/// Load index and metadata; child links are rebuilt.
pub fn load_with(path: &Path) -> Result<(Index, Meta)> {
    let mut r = BufReader::new(File::open(path)?);
    let mut head = [0u8; 12];
    r.read_exact(&mut head)?;
    let version = u32::from_le_bytes(head[8..].try_into().unwrap());
    if &head[..8] != MAGIC || version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is not a ferret v{VERSION} snapshot; re-run `ff index`",
                path.display()
            ),
        )
        .into());
    }
    let meta: Meta = bincode::deserialize_from(&mut r)?;
    let mut index: Index = bincode::deserialize_from(r)?;
    index.relink();
    Ok((index, meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::NO_PARENT;
    use crate::Kind;

    #[test]
    fn round_trip_and_bad_magic() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("sub/index.bin");
        let mut ix = Index::default();
        let r = ix.push(NO_PARENT, "/", Kind::Dir, 0, 0);
        let f = ix.push(r, "Résumé.pdf", Kind::File, 42, 7);
        save(&ix, &p).unwrap();
        let back = load(&p).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back.path(f), "/Résumé.pdf");
        assert_eq!(back.name_lower(f), "résumé.pdf".as_bytes());
        assert_eq!(back.nodes[f as usize].size, 42);
        assert_eq!(back.children(r).collect::<Vec<_>>(), vec![f]);
        let meta = Meta {
            last_event_id: 99,
            volume_uuid: "U".into(),
        };
        save_with(&back, &meta, &p).unwrap();
        let (_, m) = load_with(&p).unwrap();
        assert_eq!((m.last_event_id, m.volume_uuid.as_str()), (99, "U"));

        fs::write(&p, b"NOTFERRETxxxxxxxx").unwrap();
        let err = load(&p).err().unwrap().to_string();
        assert!(err.contains("not a ferret"), "{err}");
    }
}
