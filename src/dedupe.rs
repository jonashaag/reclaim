//! Finding files with identical content and replacing them with APFS clones of one copy.

use crate::replace::{self, DirFd, Identity, Meta, Replaced, Tmp};
use crate::scan::{File, Scan};
use crate::sys;
use rayon::prelude::*;
use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};

/// Bytes at each end of a file that are compared before the whole file. Files of the same size
/// with different content mostly differ there already (headers, code signatures), which saves
/// reading them whole.
const END: u64 = 4 * 1024;

/// Indices into `Scan::files` of files that share one data stream (pure APFS clones), or a single file.
pub type Family = Vec<usize>;

/// Families whose files all have the same content.
pub type ContentGroup = Vec<Family>;

/// What comparing files has read so far.
#[derive(Default)]
pub struct Hashed {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
}

/// Which bytes of a file to hash.
#[derive(Clone, Copy)]
enum Part {
    /// The first and the last `END` bytes.
    Ends,
    All,
}

/// Hashes `part` of `f`, if the file is unchanged since the scan.
fn hash(scan: &Scan, f: &File, part: Part, hashed: &Hashed) -> io::Result<[u8; 32]> {
    let fd = sys::open(&scan.path(f), libc::O_RDONLY | libc::O_NOFOLLOW_ANY)?;
    replace::unchanged_fd(&fd, f.into())?;
    unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_NOCACHE, 1) };
    let ranges = match part {
        Part::Ends if f.size > 2 * END => vec![(0, END), (f.size - END, f.size)],
        _ => vec![(0, f.size)],
    };
    let mut hasher = blake3::Hasher::new();
    let longest = ranges
        .iter()
        .map(|(start, end)| end - start)
        .max()
        .unwrap_or(0);
    let mut buf = vec![0u8; longest.min(1 << 20) as usize];
    for (start, end) in ranges {
        let mut offset = start;
        while offset < end {
            let want = ((end - offset) as usize).min(buf.len());
            let n = unsafe {
                libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), want, offset as i64)
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n as usize]);
            offset += n as u64;
            hashed.bytes.fetch_add(n as u64, Ordering::Relaxed);
        }
    }
    hashed.files.fetch_add(1, Ordering::Relaxed);
    Ok(*hasher.finalize().as_bytes())
}

/// Splits `families` (of same-size files) into groups whose representatives hash equally.
fn split_by_hash(
    scan: &Scan,
    families: Vec<Family>,
    part: Part,
    hashed: &Hashed,
) -> Vec<Vec<Family>> {
    let digests: Vec<Option<[u8; 32]>> = families
        .par_iter()
        .map(|fam| {
            if crate::interrupted() {
                return None;
            }
            hash(scan, &scan.files[fam[0]], part, hashed).ok()
        })
        .collect();
    let mut by_digest: HashMap<[u8; 32], Vec<Family>> = HashMap::new();
    for (fam, d) in families.into_iter().zip(digests) {
        if let Some(d) = d {
            by_digest.entry(d).or_default().push(fam);
        }
    }
    by_digest.into_values().filter(|g| g.len() > 1).collect()
}

/// Groups the files of at least `min_size` bytes into families with identical content.
///
/// Pure clones (same clone id, sharing all blocks) are one family without hashing. Families are
/// compared by hashing one representative each, first its first and last 4 KiB and then the
/// whole file.
pub fn content_groups(scan: &Scan, min_size: u64, hashed: &Hashed) -> Vec<ContentGroup> {
    let mut by_size: HashMap<(u32, u64), HashMap<(bool, u64), Family>> = HashMap::new();
    for (i, f) in scan.files.iter().enumerate() {
        if f.size < min_size {
            continue;
        }
        // A file that is not a pure clone may still have the inode number that is the clone id
        // of others, so the two kinds of keys must not mix.
        let key = if f.is_pure_clone() {
            (true, f.clone_id)
        } else {
            (false, f.ino)
        };
        by_size
            .entry((f.dev, f.size))
            .or_default()
            .entry(key)
            .or_default()
            .push(i);
    }
    let candidates: Vec<(u64, Vec<Family>)> = by_size
        .into_iter()
        .filter(|(_, fams)| fams.len() > 1)
        .map(|((_, size), fams)| (size, fams.into_values().collect()))
        .collect();
    candidates
        .into_par_iter()
        .flat_map_iter(|(size, families)| {
            let by_ends = split_by_hash(scan, families, Part::Ends, hashed);
            if size <= 2 * END {
                return by_ends;
            }
            by_ends
                .into_iter()
                .flat_map(|fams| split_by_hash(scan, fams, Part::All, hashed))
                .collect()
        })
        .collect()
}

/// How replacing a file with a clone went.
pub struct Cloned {
    pub replaced: Replaced,
    /// The identity of the clone, if nothing changed it since it took the target's place.
    pub clone: Option<Identity>,
    /// The replaced file, if it was asked to stay open.
    pub old: Option<OwnedFd>,
}

/// Replaces `target` in `dir` by a clone of the open file `source`, keeping the target's
/// metadata. Keeps the replaced file open if `keep_open`.
pub fn replace_with_clone(
    source: &OwnedFd,
    source_id: Identity,
    dir: &DirFd,
    target: &File,
    keep_open: bool,
) -> io::Result<Cloned> {
    let tfd = dir.open_file(&target.name)?;
    replace::unchanged_fd(&tfd, target.into())?;
    let meta = Meta::read(&tfd, target)?;
    let (tmp, nfd) = Tmp::clone_of(dir, source)?;
    // The clone is an atomic snapshot of the source. If the source is still unchanged now, the
    // clone has the content that was compared.
    replace::unchanged_fd(source, source_id)?;
    let compressed = sys::fstat(nfd.as_raw_fd())?.st_flags & libc::UF_COMPRESSED != 0;
    meta.apply(&nfd, compressed)?;
    let before = Identity::of(&nfd)?;
    let (replaced, old) = tmp.replace(&target.name, tfd, target.into(), keep_open)?;
    // The swap changes the clone's ctime, but not what a write would change.
    let clone = Identity::of(&nfd).ok().filter(|now| {
        now.generation != 0
            && Identity {
                ctime: before.ctime,
                ..*now
            } == before
    });
    Ok(Cloned {
        replaced,
        clone,
        old,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::tests::{scanned, scanned_dir, tempdir};
    use crate::replace::TouchedDirs;

    fn clone_over(
        source: &std::path::Path,
        source_id: Identity,
        target: &std::path::Path,
    ) -> io::Result<()> {
        let dirs = scanned_dir(target);
        let touched = TouchedDirs::new();
        let dir = DirFd::open(&dirs, 0, &touched)?;
        let fd = sys::open(source, libc::O_RDONLY)?;
        let cloned = replace_with_clone(&fd, source_id, &dir, &scanned(target), false)?;
        assert!(matches!(cloned.replaced, Replaced::Clean));
        Ok(())
    }

    #[test]
    fn refuses_a_source_that_changed_after_comparing() {
        let dir = tempdir();
        let source = dir.join("source");
        let target = dir.join("target");
        std::fs::write(&source, vec![b'a'; 100_000]).unwrap();
        std::fs::write(&target, vec![b'a'; 100_000]).unwrap();
        let source_id = Identity::from(&scanned(&source));
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&source, vec![b'b'; 100_000]).unwrap();

        assert!(clone_over(&source, source_id, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), vec![b'a'; 100_000]);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "no temporary file is left behind"
        );
    }

    #[test]
    fn clone_keeps_the_targets_metadata() {
        let dir = tempdir();
        let source = dir.join("source");
        let target = dir.join("target");
        std::fs::write(&source, vec![b'a'; 100_000]).unwrap();
        std::fs::write(&target, vec![b'a'; 100_000]).unwrap();
        let fd = sys::open(&target, libc::O_RDONLY).unwrap();
        sys::set_xattr(fd.as_raw_fd(), c"user.test", b"value", 0, 0).unwrap();
        drop(fd);
        std::process::Command::new("chmod")
            .arg("444")
            .arg(&target)
            .status()
            .unwrap();
        std::process::Command::new("touch")
            .args(["-t", "202001020304"])
            .arg(&target)
            .status()
            .unwrap();
        let before = std::fs::symlink_metadata(&target).unwrap();

        clone_over(&source, (&scanned(&source)).into(), &target).unwrap();

        let after = std::fs::symlink_metadata(&target).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_ne!(before.ino(), after.ino());
        assert_eq!(
            (before.mode(), before.mtime(), before.mtime_nsec()),
            (after.mode(), after.mtime(), after.mtime_nsec())
        );
        assert_eq!(before.created().unwrap(), after.created().unwrap());
        let fd = sys::open(&target, libc::O_RDONLY).unwrap();
        let xattrs = sys::list_xattrs(fd.as_raw_fd()).unwrap();
        assert_eq!(xattrs, vec![(c"user.test".to_owned(), b"value".to_vec())]);
        assert_eq!(
            sys::file_attrs(&target).unwrap().private_size,
            0,
            "shares all blocks with the source"
        );
    }
}
