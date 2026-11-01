//! Parallel directory walk with `getattrlistbulk`, which returns the APFS clone attributes of all
//! entries of a directory in a few system calls instead of one `stat` plus one `getattrlist` per file.

use crate::sys::{self, EF_IS_PURGEABLE, EF_IS_SYNC_ROOT, SF_DATALESS, Timespec, VDIR, VREG};
use std::collections::HashSet;
use std::ffi::{CString, OsString};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
pub struct Dir {
    pub path: PathBuf,
    pub dev: u32,
    pub ino: u64,
    pub mtime: Timespec,
}

/// A regular file that the tool may replace.
#[derive(Debug, Clone)]
pub struct File {
    pub dir: u32,
    pub name: Box<std::ffi::OsStr>,
    pub dev: u32,
    pub ino: u64,
    pub size: u64,
    pub alloc: u64,
    pub clone_id: u64,
    pub ext_flags: u64,
    pub flags: u32,
    pub ctime: Timespec,
    pub mtime: Timespec,
    pub generation: u32,
}

impl File {
    pub fn is_compressed(&self) -> bool {
        self.flags & libc::UF_COMPRESSED != 0
    }

    pub fn is_pure_clone(&self) -> bool {
        self.ext_flags & sys::EF_SHARES_ALL_BLOCKS != 0
    }

    pub fn may_share_blocks(&self) -> bool {
        self.ext_flags & (sys::EF_MAY_SHARE_BLOCKS | sys::EF_SHARES_ALL_BLOCKS) != 0
    }
}

pub struct Scan {
    pub dirs: Vec<Dir>,
    pub files: Vec<File>,
}

impl Scan {
    pub fn path(&self, f: &File) -> PathBuf {
        self.dirs[f.dir as usize].path.join(&*f.name)
    }
}

#[derive(Default, Debug)]
pub struct ScanStats {
    pub dirs: AtomicU64,
    pub regular_files: AtomicU64,
    pub logical_bytes: AtomicU64,
    pub allocated_bytes: AtomicU64,
    pub unshared_bytes: AtomicU64,
    pub skipped_files: AtomicU64,
    pub skipped_open: AtomicU64,
    pub unreadable_dirs: AtomicU64,
}

/// Flags that make a file unsafe or pointless to replace.
const BLOCKING_FLAGS: u32 = libc::UF_IMMUTABLE
    | libc::UF_APPEND
    | libc::SF_IMMUTABLE
    | libc::SF_APPEND
    | 0x0008_0000 // SF_RESTRICTED
    | 0x0010_0000 // SF_NOUNLINK
    | 0x0000_0080 // UF_DATAVAULT
    | 0x0000_0040 // UF_TRACKED: document IDs follow the inode
    | SF_DATALESS;

pub struct Options<'a> {
    pub exclude: &'a [OsString],
    pub open_for_writing: &'a HashSet<(u32, u64)>,
    /// Smaller files are counted in the stats but not added to the scan result.
    pub min_size: u64,
    /// Leave temporary files of killed runs alone.
    pub dry_run: bool,
}

thread_local! {
    static BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0u8; 128 * 1024]);
}

struct Shared<'a> {
    opts: &'a Options<'a>,
    uid: u32,
    /// `(dev, ino)` of the directories walked so far, which overlapping paths would walk again.
    walked: Mutex<HashSet<(u32, u64)>>,
    dirs: Mutex<Vec<Dir>>,
    files: Mutex<Vec<File>>,
    stats: &'a ScanStats,
}

/// Walks `roots` and updates `stats` during the walk.
pub fn scan(roots: &[PathBuf], opts: &Options, stats: &ScanStats) -> Scan {
    let shared = Shared {
        opts,
        uid: unsafe { libc::geteuid() },
        walked: Mutex::new(HashSet::new()),
        dirs: Mutex::new(Vec::new()),
        files: Mutex::new(Vec::new()),
        stats,
    };
    rayon::scope(|s| {
        for root in roots {
            // Later operations refuse symlinks anywhere in a path.
            let root = match std::fs::canonicalize(root) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("skipping {}: {e}", root.display());
                    continue;
                }
            };
            if !root.is_dir() {
                eprintln!("skipping {}: not a directory", root.display());
                continue;
            }
            match in_sync_root(&root) {
                Ok(false) => {}
                Ok(true) => {
                    eprintln!(
                        "skipping {}: inside an iCloud or File Provider sync root",
                        root.display()
                    );
                    continue;
                }
                Err(e) => {
                    eprintln!("skipping {}: {e}", root.display());
                    continue;
                }
            }
            let shared = &shared;
            s.spawn(move |s| walk(s, shared, root, None));
        }
    });
    let mut dirs = shared.dirs.into_inner().unwrap();
    let mut files = shared.files.into_inner().unwrap();
    // Give directories stable indices sorted by path, so output order is deterministic.
    let mut order: Vec<u32> = (0..dirs.len() as u32).collect();
    order.sort_by(|&a, &b| dirs[a as usize].path.cmp(&dirs[b as usize].path));
    let mut new_index = vec![0u32; dirs.len()];
    for (new, &old) in order.iter().enumerate() {
        new_index[old as usize] = new as u32;
    }
    for f in &mut files {
        f.dir = new_index[f.dir as usize];
    }
    let mut taken: Vec<Option<Dir>> = dirs.drain(..).map(Some).collect();
    let dirs = order
        .iter()
        .map(|&i| taken[i as usize].take().unwrap())
        .collect();
    files.sort_by(|a, b| (a.dir, &a.name).cmp(&(b.dir, &b.name)));
    Scan { dirs, files }
}

/// Whether `path` or one of its ancestors is a sync root.
fn in_sync_root(path: &std::path::Path) -> std::io::Result<bool> {
    for p in path.ancestors() {
        if sys::ext_flags(p)? & EF_IS_SYNC_ROOT != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `name` has the form of a temporary file of a run that no longer exists.
fn is_stale_tmp_name(name: &std::ffi::OsStr) -> bool {
    let Some(rest) = name
        .to_str()
        .and_then(|n| n.strip_prefix(crate::replace::TMP_MARKER))
    else {
        return false;
    };
    let Some((pid, n)) = rest.split_once('.') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(pid) || !digits(n) {
        return false;
    }
    let Ok(pid) = pid.parse::<libc::pid_t>() else {
        return false;
    };
    pid != std::process::id() as libc::pid_t
        && unsafe { libc::kill(pid, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// The name and inode of the entry `name` of the directory `dir`, if it is the regular file with
/// the inode `ino` that this tool created as a temporary file and nothing else links to.
fn own_tmp(dir: &OwnedFd, name: &std::ffi::OsStr, ino: u64, uid: u32) -> Option<(CString, u64)> {
    let name = crate::replace::cname(name);
    let fd = sys::openat(
        dir.as_raw_fd(),
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
    )
    .ok()?;
    let st = sys::fstat(fd.as_raw_fd()).ok()?;
    let ours = st.st_mode & libc::S_IFMT == libc::S_IFREG
        && st.st_ino == ino
        && st.st_uid == uid
        && st.st_nlink == 1
        && sys::has_xattr(fd.as_raw_fd(), crate::replace::TMP_XATTR);
    ours.then_some((name, ino))
}

/// Walks the directory `path`; `parent_dev` is the device of the parent directory, if any.
fn walk<'s>(s: &rayon::Scope<'s>, shared: &'s Shared<'s>, path: PathBuf, parent_dev: Option<u32>) {
    if crate::interrupted() {
        return;
    }
    let fd = match sys::open(
        &path,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW_ANY,
    ) {
        Ok(fd) => fd,
        Err(_) => {
            shared.stats.unreadable_dirs.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let Ok(st) = sys::fstat(fd.as_raw_fd()) else {
        shared.stats.unreadable_dirs.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let dev = st.st_dev as u32;
    if parent_dev.is_some_and(|d| d != dev)
        || !shared.walked.lock().unwrap().insert((dev, st.st_ino))
    {
        return;
    }
    let mtime = sys::mtime_of(&st);
    shared.stats.dirs.fetch_add(1, Ordering::Relaxed);
    let mut subdirs = Vec::new();
    let mut files = Vec::new();
    let mut stale = Vec::new();
    let stats = shared.stats;
    let res = BUF.with_borrow_mut(|buf| {
        sys::list_dir(fd.as_raw_fd(), buf, |e| {
            if e.error != 0 {
                return;
            }
            if e.objtype == VDIR {
                if e.dev == dev
                    && e.ext_flags & EF_IS_SYNC_ROOT == 0
                    && !shared.opts.exclude.iter().any(|x| x.as_os_str() == e.name)
                {
                    subdirs.push(path.join(e.name));
                }
                return;
            }
            if e.objtype != VREG {
                return;
            }
            let excluded = shared.opts.exclude.iter().any(|x| x.as_os_str() == e.name);
            if !shared.opts.dry_run
                && !excluded
                && e.uid == shared.uid
                && e.nlink == 1
                && is_stale_tmp_name(e.name)
                && let Some(t) = own_tmp(&fd, e.name, e.ino, shared.uid)
            {
                stale.push(t);
                return;
            }
            stats.regular_files.fetch_add(1, Ordering::Relaxed);
            stats.logical_bytes.fetch_add(e.size, Ordering::Relaxed);
            stats.allocated_bytes.fetch_add(e.alloc, Ordering::Relaxed);
            if e.ext_flags & (sys::EF_MAY_SHARE_BLOCKS | sys::EF_SHARES_ALL_BLOCKS) == 0 {
                stats.unshared_bytes.fetch_add(e.alloc, Ordering::Relaxed);
            }
            // An uncompressed file of one block cannot get smaller. Compressed files are kept in the
            // scan result as clone sources.
            if e.size < shared.opts.min_size
                || (e.alloc <= 4096 && e.flags & libc::UF_COMPRESSED == 0)
            {
                return;
            }
            if e.nlink != 1
                || e.uid != shared.uid
                || e.flags & BLOCKING_FLAGS != 0
                || e.ext_flags & EF_IS_PURGEABLE != 0
                || excluded
            {
                stats.skipped_files.fetch_add(1, Ordering::Relaxed);
                return;
            }
            // The generation count is 0 while a process has the file mapped for writing.
            if e.generation == 0 || shared.opts.open_for_writing.contains(&(e.dev, e.ino)) {
                stats.skipped_open.fetch_add(1, Ordering::Relaxed);
                return;
            }
            files.push(File {
                dir: 0,
                name: e.name.into(),
                dev: e.dev,
                ino: e.ino,
                size: e.size,
                alloc: e.alloc,
                clone_id: e.clone_id,
                ext_flags: e.ext_flags,
                flags: e.flags,
                ctime: e.ctime,
                mtime: e.mtime,
                generation: e.generation,
            });
        })
    });
    for (name, ino) in stale {
        #[cfg(test)]
        if let Some(f) = tests::BEFORE_CLEANUP.lock().unwrap().take() {
            f();
        }
        if sys::fstatat(fd.as_raw_fd(), &name).is_ok_and(|st| st.st_ino == ino) {
            let _ = sys::unlinkat(fd.as_raw_fd(), &name);
        }
    }
    drop(fd);
    if res.is_err() {
        shared.stats.unreadable_dirs.fetch_add(1, Ordering::Relaxed);
    }
    if !files.is_empty() {
        let index = {
            let mut dirs = shared.dirs.lock().unwrap();
            dirs.push(Dir {
                path: path.clone(),
                dev,
                ino: st.st_ino,
                mtime,
            });
            (dirs.len() - 1) as u32
        };
        for f in &mut files {
            f.dir = index;
        }
        shared.files.lock().unwrap().append(&mut files);
    }
    for sub in subdirs {
        s.spawn(move |s| walk(s, shared, sub, Some(dev)));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Runs before the next removal of a temporary file of an earlier run.
    pub(crate) static BEFORE_CLEANUP: Mutex<Option<Box<dyn FnOnce() + Send>>> = Mutex::new(None);

    #[test]
    fn cleanup_stays_in_the_scanned_directory() {
        let base = std::fs::canonicalize(crate::compress::tests::tempdir()).unwrap();
        let (root, moved, outside) = (base.join("root"), base.join("moved"), base.join("outside"));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let name = ".reclaim-tmp.99999999.0";
        std::fs::write(root.join(name), b"temporary").unwrap();
        let fd = sys::open(&root.join(name), libc::O_RDONLY).unwrap();
        sys::set_xattr(fd.as_raw_fd(), crate::replace::TMP_XATTR, b"", 0, 0).unwrap();
        drop(fd);
        std::fs::write(outside.join(name), b"not ours").unwrap();
        let (r, m, o) = (root.clone(), moved.clone(), outside.clone());
        *BEFORE_CLEANUP.lock().unwrap() = Some(Box::new(move || {
            std::fs::rename(&r, m).unwrap();
            std::os::unix::fs::symlink(o, r).unwrap();
        }));
        let options = Options {
            exclude: &[],
            open_for_writing: &HashSet::new(),
            min_size: 1,
            dry_run: false,
        };
        scan(&[root], &options, &ScanStats::default());

        assert!(outside.join(name).exists());
        assert!(!moved.join(name).exists());
    }
}
