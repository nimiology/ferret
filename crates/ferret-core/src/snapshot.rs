use crate::{Index, Result};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"FERRET01";
const VERSION: u32 = 1;

/// `~/Library/Caches/ferret/index.bin`
pub fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join("Library/Caches/ferret/index.bin")
}

/// Write atomically (temp file + rename).
pub fn save(index: &Index, path: &Path) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let res = (|| {
        let mut w = BufWriter::new(File::create(&tmp)?);
        w.write_all(MAGIC)?;
        w.write_all(&VERSION.to_le_bytes())?;
        bincode::serialize_into(&mut w, index)?;
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
    let mut r = BufReader::new(File::open(path)?);
    let mut head = [0u8; 12];
    r.read_exact(&mut head)?;
    let version = u32::from_le_bytes(head[8..].try_into().unwrap());
    if &head[..8] != MAGIC || version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a ferret v{VERSION} snapshot; re-run `ff index`", path.display()),
        )
        .into());
    }
    Ok(bincode::deserialize_from(r)?)
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

        fs::write(&p, b"NOTFERRETxxxxxxxx").unwrap();
        let err = load(&p).err().unwrap().to_string();
        assert!(err.contains("not a ferret"), "{err}");
    }
}
