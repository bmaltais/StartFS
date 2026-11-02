//! The metadata database on disk, encrypted: a [redb] storage backend that keeps the file as
//! [`SECTOR`]-byte units under the filesystem key (ADR 0010).
//!
//! Encrypting the file, not the rows, hides everything the database holds, including file
//! names, which are table keys, and the database's own structure. A sector never written is
//! a hole in the file and reads back as zeros, as redb expects of new space.

use crate::crypto::{SECTOR, SectorCipher};
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::RwLock;

const S: u64 = SECTOR as u64;

#[derive(Debug)]
pub struct EncryptedFile {
    file: File,
    cipher: SectorCipher,
    /// Writes patch whole sectors, so they must not interleave with each other or with reads.
    gate: RwLock<()>,
}

impl EncryptedFile {
    pub fn open(path: &Path, cipher: SectorCipher) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        Ok(Self {
            file,
            cipher,
            gate: RwLock::new(()),
        })
    }

    /// The plaintext of sectors `first..first + count`.
    fn read_sectors(&self, first: u64, count: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; count * SECTOR];
        self.file.read_exact_at(&mut buf, first * S)?;
        for (i, sector) in buf.as_chunks_mut::<SECTOR>().0.iter_mut().enumerate() {
            if sector.iter().any(|b| *b != 0) {
                self.cipher.decrypt(sector, first + i as u64);
            }
        }
        Ok(buf)
    }
}

impl redb::StorageBackend for EncryptedFile {
    fn len(&self) -> io::Result<u64> {
        self.file.metadata().map(|m| m.len())
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        if out.is_empty() {
            return Ok(());
        }
        let _shared = self.gate.read().unwrap();
        let first = offset / S;
        let last = (offset + out.len() as u64 - 1) / S;
        let plain = self.read_sectors(first, (last - first + 1) as usize)?;
        let skip = (offset - first * S) as usize;
        out.copy_from_slice(&plain[skip..skip + out.len()]);
        Ok(())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        let _exclusive = self.gate.write().unwrap();
        self.file.set_len(len.div_ceil(S) * S)
    }

    fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let _exclusive = self.gate.write().unwrap();
        let first = offset / S;
        let last = (offset + data.len() as u64 - 1) / S;
        let count = (last - first + 1) as usize;
        let physical = self.file.metadata()?.len();
        if (last + 1) * S > physical {
            self.file.set_len((last + 1) * S)?;
        }
        // Sectors only partly covered keep the rest of what they hold.
        let skip = (offset - first * S) as usize;
        let mut plain = if skip == 0 && data.len().is_multiple_of(SECTOR) {
            vec![0u8; count * SECTOR]
        } else {
            self.read_sectors(first, count)?
        };
        plain[skip..skip + data.len()].copy_from_slice(data);
        for (i, sector) in plain.as_chunks_mut::<SECTOR>().0.iter_mut().enumerate() {
            self.cipher.encrypt(sector, first + i as u64);
        }
        self.file.write_all_at(&plain, first * S)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::FsKey;
    use redb::{Database, ReadableDatabase, TableDefinition};

    const T: TableDefinition<&str, &[u8]> = TableDefinition::new("t");

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sfmeta-{name}-{}", std::process::id()));
        #[allow(
            clippy::let_underscore_must_use,
            reason = "a leftover from an earlier run"
        )]
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn open(path: &Path, key: &FsKey) -> Result<Database, redb::DatabaseError> {
        Database::builder()
            .create_with_backend(EncryptedFile::open(path, key.sector_cipher()).unwrap())
    }

    #[test]
    fn a_database_keeps_its_rows_and_its_file_holds_no_names() {
        let d = dir("rows");
        let (path, key) = (d.join("meta.enc"), FsKey::generate());
        {
            let db = open(&path, &key).unwrap();
            for round in 0..40 {
                let txn = db.begin_write().unwrap();
                {
                    let mut t = txn.open_table(T).unwrap();
                    for i in 0..50 {
                        let name = format!("quarterly-report-{round}-{i}.txt");
                        t.insert(name.as_str(), b"value".as_slice()).unwrap();
                    }
                }
                txn.commit().unwrap();
            }
        }
        let raw = std::fs::read(&path).unwrap();
        assert!(raw.len() > 100_000);
        assert!(!raw.windows(16).any(|w| w == b"quarterly-report"));
        assert!(!raw.windows(4).any(|w| w == b"redb"));

        let db = open(&path, &key).unwrap();
        let t = db.begin_read().unwrap().open_table(T).unwrap();
        assert!(
            redb::ReadableTable::get(&t, "quarterly-report-39-49.txt")
                .unwrap()
                .is_some()
        );
        drop(t);
        drop(db);
        assert!(open(&path, &FsKey::generate()).is_err(), "wrong key");
        std::fs::remove_dir_all(d).unwrap();
    }
}
