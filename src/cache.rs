//! Remembers files that did not compress well enough, so that later runs do not read and
//! compress them again. A file is identified by device, inode, ctime and size, so any change to
//! it makes it a candidate again.

use crate::scan::File;
use crate::sys::Timespec;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Mutex;

const MAGIC: &[u8; 16] = b"reclaim-incomp-1";
const RECORD: usize = 4 + 8 + 8 + 8 + 8 + 4;
/// Entries not seen for this many days are dropped, which removes deleted files.
const KEEP_DAYS: u32 = 60;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    dev: u32,
    ino: u64,
    ctime: Timespec,
    size: u64,
}

impl From<&File> for Key {
    fn from(f: &File) -> Self {
        Key {
            dev: f.dev,
            ino: f.ino,
            ctime: f.ctime,
            size: f.size,
        }
    }
}

pub struct Incompressible {
    path: Option<PathBuf>,
    settings: u64,
    /// Day of the last run that found each file, counted from the Unix epoch.
    entries: HashMap<Key, u32>,
    seen: Mutex<Vec<Key>>,
    added: Mutex<Vec<Key>>,
}

fn today() -> u32 {
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 86400) as u32
}

pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches/reclaim/incompressible"))
}

impl Incompressible {
    /// Loads the cache; `settings` identifies the compression settings the entries are valid for.
    pub fn load(path: Option<PathBuf>, settings: u64) -> Self {
        let entries = path
            .as_ref()
            .and_then(|p| read(p, settings).ok())
            .unwrap_or_default();
        Incompressible {
            path,
            settings,
            entries,
            seen: Mutex::default(),
            added: Mutex::default(),
        }
    }

    pub fn contains(&self, f: &File) -> bool {
        let key = Key::from(f);
        let hit = self.entries.contains_key(&key);
        if hit {
            self.seen.lock().unwrap().push(key);
        }
        hit
    }

    pub fn insert(&self, f: &File) {
        self.added.lock().unwrap().push(f.into());
    }

    pub fn save(self) -> io::Result<()> {
        let Some(path) = self.path else { return Ok(()) };
        let today = today();
        let mut entries = self.entries;
        for key in self
            .seen
            .into_inner()
            .unwrap()
            .into_iter()
            .chain(self.added.into_inner().unwrap())
        {
            entries.insert(key, today);
        }
        let mut buf = Vec::with_capacity(24 + entries.len() * RECORD);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&self.settings.to_le_bytes());
        for (k, day) in entries {
            if today.saturating_sub(day) > KEEP_DAYS {
                continue;
            }
            buf.extend_from_slice(&k.dev.to_le_bytes());
            buf.extend_from_slice(&k.ino.to_le_bytes());
            buf.extend_from_slice(&k.ctime.sec.to_le_bytes());
            buf.extend_from_slice(&k.ctime.nsec.to_le_bytes());
            buf.extend_from_slice(&k.size.to_le_bytes());
            buf.extend_from_slice(&day.to_le_bytes());
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        // Exclusive creation neither follows a symlink nor truncates a file that is already there.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        if let Err(e) = file
            .write_all(&buf)
            .and_then(|()| std::fs::rename(&tmp, &path))
        {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
    }
}

fn read(path: &PathBuf, settings: u64) -> io::Result<HashMap<Key, u32>> {
    let mut buf = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut buf)?;
    if buf.len() < 24 || &buf[..16] != MAGIC || buf[16..24] != settings.to_le_bytes() {
        return Ok(HashMap::new());
    }
    let u64_at = |r: &[u8], i: usize| u64::from_le_bytes(r[i..i + 8].try_into().unwrap());
    Ok(buf[24..]
        .chunks_exact(RECORD)
        .map(|r| {
            let key = Key {
                dev: u32::from_le_bytes(r[0..4].try_into().unwrap()),
                ino: u64_at(r, 4),
                ctime: Timespec {
                    sec: u64_at(r, 12) as i64,
                    nsec: u64_at(r, 20) as i64,
                },
                size: u64_at(r, 28),
            };
            (key, u32::from_le_bytes(r[36..40].try_into().unwrap()))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_does_not_write_through_a_symlink() {
        let dir = crate::compress::tests::tempdir();
        let valuable = dir.join("valuable");
        std::fs::write(&valuable, b"precious").unwrap();
        let path = dir.join("incompressible");
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::os::unix::fs::symlink(&valuable, &tmp).unwrap();

        assert!(Incompressible::load(Some(path.clone()), 1).save().is_err());
        assert_eq!(std::fs::read(&valuable).unwrap(), b"precious");
        assert!(std::fs::symlink_metadata(&tmp).unwrap().is_symlink());
    }
}
