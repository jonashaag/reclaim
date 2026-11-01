//! Atomic replacement of a file by a new file (a clone or a compressed copy) that keeps the
//! replaced file's metadata.
//!
//! All operations go through a descriptor of the file's directory, which was checked to be the
//! directory from the scan. Renaming a directory during the run therefore cannot make them act on
//! another directory.

use crate::scan::{Dir, File};
use crate::sys::{self, Timespec};
use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

pub const TMP_MARKER: &str = ".reclaim-tmp.";
/// Marks a temporary file as created by this tool until the replaced file's extended attributes
/// overwrite it, so that a later run only removes files it owns.
pub const TMP_XATTR: &CStr = c"io.github.jonashaag.reclaim.tmp";
/// Suffix of a replaced file that changed while it was being replaced.
pub const CHANGED_SUFFIX: &str = ".reclaim-changed";

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn cname(name: &OsStr) -> CString {
    CString::new(name.as_bytes()).expect("file name contains a NUL byte")
}

/// An open directory, checked to be the directory that the scan found at its path.
pub struct DirFd<'a> {
    fd: OwnedFd,
    index: u32,
    pub dir: &'a Dir,
    touched: &'a TouchedDirs,
}

impl<'a> DirFd<'a> {
    pub fn open(dirs: &'a [Dir], index: u32, touched: &'a TouchedDirs) -> io::Result<Self> {
        let dir = &dirs[index as usize];
        let fd = sys::open(
            &dir.path,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW_ANY,
        )?;
        let st = sys::fstat(fd.as_raw_fd())?;
        if st.st_ino != dir.ino || st.st_dev as u32 != dir.dev {
            return Err(changed());
        }
        Ok(DirFd {
            fd,
            index,
            dir,
            touched,
        })
    }

    pub fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Opens the file `name` in this directory for reading, refusing symlinks.
    pub fn open_file(&self, name: &OsStr) -> io::Result<OwnedFd> {
        sys::openat(self.raw(), &cname(name), libc::O_RDONLY | libc::O_NOFOLLOW)
    }

    fn ino_of(&self, name: &CStr) -> Option<u64> {
        sys::fstatat(self.raw(), name).ok().map(|st| st.st_ino)
    }

    pub fn path_of(&self, name: &CStr) -> PathBuf {
        self.dir.path.join(OsStr::from_bytes(name.to_bytes()))
    }

    /// Runs `op`, which changes this directory's entries, so that the directory's modification
    /// time can be restored afterwards if nothing else changed the directory.
    fn modify<T>(&self, op: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        self.touched.modify(self, op)
    }
}

/// A temporary file next to the file it will replace, removed on drop unless it was swapped in.
pub struct Tmp<'d> {
    dir: &'d DirFd<'d>,
    name: CString,
    ino: u64,
    done: bool,
}

impl<'d> Tmp<'d> {
    /// Creates a temporary file with `create`, which gets a free name and must fail with
    /// `EEXIST` rather than replace an existing file.
    fn create(
        dir: &'d DirFd<'d>,
        create: impl Fn(&CStr) -> io::Result<OwnedFd>,
    ) -> io::Result<(Tmp<'d>, OwnedFd)> {
        let mut attempts = 0;
        let (name, fd) = loop {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = CString::new(format!("{TMP_MARKER}{}.{n}", std::process::id())).unwrap();
            match dir.modify(|| create(&name)) {
                Ok(fd) => break (name, fd),
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) && attempts < 100 => {
                    attempts += 1;
                }
                Err(e) => return Err(e),
            }
        };
        let st = match sys::fstat(fd.as_raw_fd()) {
            Ok(st) => st,
            Err(e) => {
                let _ = dir.modify(|| sys::unlinkat(dir.raw(), &name));
                return Err(e);
            }
        };
        let tmp = Tmp {
            dir,
            name,
            ino: st.st_ino,
            done: false,
        };
        // A clone has the mode of its source, which may deny writing extended attributes.
        sys::fchmod(fd.as_raw_fd(), 0o600)?;
        sys::set_xattr(fd.as_raw_fd(), TMP_XATTR, b"", 0, 0)?;
        Ok((tmp, fd))
    }

    /// Creates an empty temporary file for reading and writing.
    pub fn new_file(dir: &'d DirFd<'d>) -> io::Result<(Tmp<'d>, OwnedFd)> {
        Tmp::create(dir, |name| {
            sys::openat(
                dir.raw(),
                name,
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            )
        })
    }

    /// Creates a clone of the open file `source`, opened read-only (its mode may deny writing).
    pub fn clone_of(dir: &'d DirFd<'d>, source: &OwnedFd) -> io::Result<(Tmp<'d>, OwnedFd)> {
        Tmp::create(dir, |name| {
            sys::fclonefileat(source.as_raw_fd(), dir.raw(), name)?;
            sys::openat(dir.raw(), name, libc::O_RDONLY | libc::O_NOFOLLOW).inspect_err(|_| {
                let _ = sys::unlinkat(dir.raw(), name);
            })
        })
    }

    /// Puts this file in place of `name`, if `name` still is the open file `orig` with the
    /// identity `expected`. Returns `orig` if `keep_open`, otherwise closes it.
    ///
    /// The check and the swap are two steps, so both files are checked once more after the swap.
    /// If the replaced file changed in between, it is kept next to the new file instead of being
    /// removed. If another file took this file's name, it is swapped back.
    pub fn replace(
        mut self,
        name: &OsStr,
        orig: OwnedFd,
        expected: Identity,
        keep_open: bool,
    ) -> io::Result<(Replaced, Option<OwnedFd>)> {
        let dir = self.dir;
        let cname = cname(name);
        if dir.ino_of(&cname) != Some(expected.ino) || dir.ino_of(&self.name) != Some(self.ino) {
            return Err(changed());
        }
        let before = State::read(&orig)?;
        unchanged_fd(&orig, expected)?;
        #[cfg(test)]
        tests::BEFORE_SWAP.with_borrow_mut(|f| f.take().map(|f| f()));
        dir.modify(|| sys::swap(dir.raw(), &self.name, &cname))?;
        self.done = true;
        if dir.ino_of(&cname) != Some(self.ino) {
            let other = dir.ino_of(&self.name);
            dir.touched.keep_changed(dir);
            if dir
                .modify(|| sys::swap(dir.raw(), &self.name, &cname))
                .is_ok()
                && other.is_some()
                && dir.ino_of(&cname) == other
            {
                return Err(changed());
            }
            return Ok((self.keep(&cname), keep_open.then_some(orig)));
        }
        // The swap changed the replaced file's ctime, so only the rest of its state shows whether
        // it changed since the check.
        let swapped_out = dir.ino_of(&self.name);
        let unchanged = State::read(&orig).is_ok_and(|now| now == before);
        // APFS frees the blocks of a removed file that is still open only some time after it is
        // closed, so it is closed first unless the caller needs it.
        let orig = keep_open.then_some(orig);
        if swapped_out == Some(expected.ino)
            && unchanged
            && dir.modify(|| sys::unlinkat(dir.raw(), &self.name)).is_ok()
        {
            return Ok((Replaced::Clean, orig));
        }
        Ok((self.keep(&cname), orig))
    }

    /// Keeps the file at this file's name as `<name>.reclaim-changed`, or `<name>.reclaim-changed.<n>`
    /// if that name is taken, or where it is if no such name is free.
    fn keep(&self, name: &CStr) -> Replaced {
        let dir = self.dir;
        let base = [name.to_bytes(), CHANGED_SUFFIX.as_bytes()].concat();
        let free = (1..100).find_map(|n| {
            let kept = if n == 1 {
                CString::new(base.clone()).unwrap()
            } else {
                CString::new([&base[..], format!(".{n}").as_bytes()].concat()).unwrap()
            };
            match dir.modify(|| sys::rename_excl(dir.raw(), &self.name, &kept)) {
                Ok(()) => Some(Some(kept)),
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => None,
                Err(_) => Some(None),
            }
        });
        let kept = free.flatten().unwrap_or_else(|| self.name.clone());
        dir.touched.keep_changed(dir);
        Replaced::KeptChanged(dir.path_of(&kept))
    }
}

impl Drop for Tmp<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if self.dir.ino_of(&self.name) == Some(self.ino) {
            let _ = self
                .dir
                .modify(|| sys::unlinkat(self.dir.raw(), &self.name));
        }
    }
}

/// How a replacement went.
#[must_use]
pub enum Replaced {
    Clean,
    /// The replaced file changed between the last check and the swap, or another file took the
    /// new file's name. The file that was swapped out was kept at this path.
    KeptChanged(PathBuf),
}

#[derive(Debug)]
pub struct Changed;

impl std::fmt::Display for Changed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("file changed during the run")
    }
}

impl std::error::Error for Changed {}

pub fn changed() -> io::Error {
    io::Error::other(Changed)
}

/// What identifies a file's content at one point in time. Writes through descriptors change the
/// ctime and the generation count, and user processes cannot set either. While a process has the
/// file mapped for writing, the generation count is 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub ino: u64,
    pub ctime: Timespec,
    pub mtime: Timespec,
    pub size: u64,
    pub generation: u32,
}

impl From<&File> for Identity {
    fn from(f: &File) -> Self {
        Identity {
            ino: f.ino,
            ctime: f.ctime,
            mtime: f.mtime,
            size: f.size,
            generation: f.generation,
        }
    }
}

impl Identity {
    pub fn of(fd: &OwnedFd) -> io::Result<Identity> {
        let st = sys::fstat(fd.as_raw_fd())?;
        Ok(Identity {
            ino: st.st_ino,
            ctime: sys::ctime_of(&st),
            mtime: sys::mtime_of(&st),
            size: st.st_size as u64,
            generation: sys::gen_count(fd.as_raw_fd())?,
        })
    }
}

/// Fails if the open file `fd` is no longer the same, unmodified file as `expected`, or if a
/// process has it mapped for writing.
pub fn unchanged_fd(fd: &OwnedFd, expected: Identity) -> io::Result<()> {
    let now = Identity::of(fd)?;
    if now.generation == 0 {
        return Err(unsupported("mapped for writing"));
    }
    if now != expected {
        return Err(changed());
    }
    Ok(())
}

/// Everything about a file that a write or a change of its permissions changes, except the ctime.
#[derive(PartialEq)]
struct State {
    size: i64,
    mtime: Timespec,
    generation: u32,
    mode: u16,
    uid: u32,
    gid: u32,
    flags: u32,
    acl: bool,
    xattrs: sys::Xattrs,
}

impl State {
    fn read(fd: &OwnedFd) -> io::Result<State> {
        let raw = fd.as_raw_fd();
        let st = sys::fstat(raw)?;
        let generation = sys::gen_count(raw)?;
        if generation == 0 {
            return Err(unsupported("mapped for writing"));
        }
        Ok(State {
            size: st.st_size,
            mtime: sys::mtime_of(&st),
            generation,
            mode: st.st_mode,
            uid: st.st_uid,
            gid: st.st_gid,
            flags: st.st_flags,
            acl: sys::has_acl(raw)?,
            xattrs: sys::list_xattrs(raw)?,
        })
    }
}

/// Metadata of the replaced file that the replacement must carry.
pub struct Meta {
    mode: u32,
    uid: u32,
    gid: u32,
    flags: u32,
    times: sys::Times,
    xattrs: sys::Xattrs,
}

#[derive(Debug)]
pub struct Unsupported(pub &'static str);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Unsupported {}

pub fn unsupported(reason: &'static str) -> io::Error {
    io::Error::other(Unsupported(reason))
}

fn acl_check(fd: RawFd, reason: &'static str) -> io::Result<()> {
    match sys::has_acl(fd) {
        Ok(false) => Ok(()),
        Ok(true) => Err(unsupported(reason)),
        Err(_) => Err(unsupported("ACL cannot be read")),
    }
}

impl Meta {
    /// Reads the metadata of an open file, refusing files whose metadata a replacement cannot keep.
    pub fn read(fd: &OwnedFd, f: &File) -> io::Result<Meta> {
        let raw = fd.as_raw_fd();
        acl_check(raw, "has an ACL")?;
        let st = sys::fstat(raw)?;
        let xattrs = if f.ext_flags & sys::EF_NO_XATTRS != 0 {
            Vec::new()
        } else {
            sys::list_xattrs(raw)?
        };
        if xattrs
            .iter()
            .any(|(n, _)| n.as_c_str() == sys::XATTR_RESOURCE_FORK)
        {
            return Err(unsupported("has a resource fork"));
        }
        Ok(Meta {
            mode: st.st_mode as u32,
            uid: st.st_uid,
            gid: st.st_gid,
            flags: st.st_flags,
            times: sys::get_times(raw)?,
            xattrs,
        })
    }

    /// Applies this metadata to `fd`; `compressed` says whether the new file's data is compressed.
    /// This also removes the temporary file marker.
    pub fn apply(&self, fd: &OwnedFd, compressed: bool) -> io::Result<()> {
        let raw = fd.as_raw_fd();
        acl_check(raw, "clone source has an ACL")?;
        sys::sync_xattrs(raw, &self.xattrs)?;
        let st = sys::fstat(raw)?;
        if st.st_uid != self.uid || st.st_gid != self.gid {
            sys::fchown(raw, self.uid, self.gid)?;
        }
        let compressed_flag = if compressed { libc::UF_COMPRESSED } else { 0 };
        let flags = (self.flags & !libc::UF_COMPRESSED) | compressed_flag;
        if st.st_flags != flags {
            sys::fchflags(raw, flags)?;
        }
        sys::fchmod(raw, self.mode)?;
        sys::set_times(raw, &self.times)
    }
}

#[derive(Clone, Copy)]
struct DirState {
    /// The modification time after the run's last change.
    last: Timespec,
    /// Something else changed the directory, or the run left a file in it.
    foreign: bool,
}

/// Directories whose entries the run changed, and whether only the run changed them.
pub struct TouchedDirs {
    state: Mutex<HashMap<u32, DirState>>,
    /// Serializes the changes to one directory, so that each change of its modification time can
    /// be attributed to one operation.
    stripes: Vec<Mutex<()>>,
}

impl TouchedDirs {
    pub fn new() -> Self {
        Self {
            state: Mutex::default(),
            stripes: (0..256).map(|_| Mutex::new(())).collect(),
        }
    }

    fn modify<T>(&self, dir: &DirFd, op: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let _guard = self.stripes[dir.index as usize % self.stripes.len()]
            .lock()
            .unwrap();
        let mtime = || sys::fstat(dir.raw()).ok().map(|st| sys::mtime_of(&st));
        let before = mtime();
        let res = op();
        let after = mtime();
        let mut state = self.state.lock().unwrap();
        let prev = state.get(&dir.index).copied();
        let expected = prev.map_or(dir.dir.mtime, |s| s.last);
        state.insert(
            dir.index,
            DirState {
                last: after.unwrap_or_default(),
                foreign: prev.is_some_and(|s| s.foreign)
                    || before != Some(expected)
                    || after.is_none(),
            },
        );
        res
    }

    fn keep_changed(&self, dir: &DirFd) {
        if let Some(s) = self.state.lock().unwrap().get_mut(&dir.index) {
            s.foreign = true;
        }
    }

    /// Restores the modification times of the directories that only the run changed.
    pub fn restore(&self, dirs: &[Dir]) {
        let state = std::mem::take(&mut *self.state.lock().unwrap());
        for (index, s) in state {
            if s.foreign {
                continue;
            }
            let Ok(d) = DirFd::open(dirs, index, self) else {
                continue;
            };
            if sys::fstat(d.raw()).is_ok_and(|st| sys::mtime_of(&st) == s.last) {
                let _ = sys::set_mtime_fd(d.raw(), d.dir.mtime);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::compress::tests::{scanned, scanned_dir, tempdir};
    use std::cell::RefCell;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    thread_local! {
        /// Runs between the last check and the swap of the next replacement on this thread.
        pub(crate) static BEFORE_SWAP: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    /// The number the next temporary file name will have, if no other thread takes it first.
    pub(crate) fn next_tmp_number() -> u64 {
        COUNTER.load(Ordering::Relaxed)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn dir_mtime(dir: &Path) -> Timespec {
        let fd = sys::open(dir, libc::O_RDONLY).unwrap();
        sys::mtime_of(&sys::fstat(fd.as_raw_fd()).unwrap())
    }

    /// Replaces `path` with a new file containing `new`, running `before_swap` between the last
    /// check and the swap. Returns how it went and the directory's modification time afterwards.
    fn replace_with(
        path: &Path,
        new: &[u8],
        before_swap: impl FnOnce() + 'static,
    ) -> (Replaced, Timespec) {
        let dirs = scanned_dir(path);
        let touched = TouchedDirs::new();
        let f = scanned(path);
        let replaced = {
            let dir = DirFd::open(&dirs, 0, &touched).unwrap();
            let orig = dir.open_file(&f.name).unwrap();
            let (tmp, tfd) = Tmp::new_file(&dir).unwrap();
            std::fs::File::from(tfd).write_all(new).unwrap();
            BEFORE_SWAP.set(Some(Box::new(before_swap)));
            let (replaced, _) = tmp.replace(&f.name, orig, (&f).into(), false).unwrap();
            replaced
        };
        touched.restore(&dirs);
        (replaced, dir_mtime(path.parent().unwrap()))
    }

    #[test]
    fn restores_the_directory_modification_time() {
        let dir = tempdir();
        let path = dir.join("file");
        std::fs::write(&path, b"old").unwrap();
        let before = dir_mtime(&dir);

        let (replaced, after) = replace_with(&path, b"new", || {});
        assert!(matches!(replaced, Replaced::Clean));
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(names(&dir), ["file"]);
        assert_eq!(after, before);
    }

    #[test]
    fn keeps_a_file_written_to_during_the_replacement() {
        let dir = tempdir();
        let path = dir.join("file");
        std::fs::write(&path, b"old").unwrap();
        let p = path.clone();
        let (replaced, _) = replace_with(&path, b"new", move || {
            let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
            f.write_all(b" and appended").unwrap();
            f.sync_all().unwrap();
        });

        let kept = dir.join("file.reclaim-changed");
        assert!(
            matches!(replaced, Replaced::KeptChanged(ref p) if *p == std::fs::canonicalize(&kept).unwrap())
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read(&kept).unwrap(), b"old and appended");
        assert_eq!(names(&dir), ["file", "file.reclaim-changed"]);
    }

    #[test]
    fn keeps_a_file_saved_over_it_during_the_replacement() {
        let dir = tempdir();
        let path = dir.join("file");
        std::fs::write(&path, b"old").unwrap();
        let before = dir_mtime(&dir);
        let (p, d) = (path.clone(), dir.clone());
        let (replaced, after) = replace_with(&path, b"new", move || {
            std::fs::write(d.join("saved"), b"saved by an editor").unwrap();
            std::fs::rename(d.join("saved"), p).unwrap();
        });

        assert!(matches!(replaced, Replaced::KeptChanged(_)));
        assert_eq!(
            std::fs::read(dir.join("file.reclaim-changed")).unwrap(),
            b"saved by an editor"
        );
        assert_ne!(after, before, "the directory changed, so its time stays");
    }

    #[test]
    fn keeps_a_file_written_to_with_its_old_modification_time() {
        let dir = tempdir();
        let path = dir.join("file");
        std::fs::write(&path, b"old").unwrap();
        let mtime = sys::mtime_of(
            &sys::fstat(sys::open(&path, libc::O_RDONLY).unwrap().as_raw_fd()).unwrap(),
        );
        let p = path.clone();
        let (replaced, _) = replace_with(&path, b"new", move || {
            std::fs::write(&p, b"OLD").unwrap();
            let fd = sys::open(&p, libc::O_RDWR).unwrap();
            sys::set_mtime_fd(fd.as_raw_fd(), mtime).unwrap();
        });

        assert!(matches!(replaced, Replaced::KeptChanged(_)));
        assert_eq!(
            std::fs::read(dir.join("file.reclaim-changed")).unwrap(),
            b"OLD"
        );
    }

    #[test]
    fn keeps_a_file_whose_permissions_changed() {
        let dir = tempdir();
        let path = dir.join("file");
        std::fs::write(&path, b"old").unwrap();
        let p = path.clone();
        let (replaced, _) = replace_with(&path, b"new", move || {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        });

        assert!(matches!(replaced, Replaced::KeptChanged(_)));
        let kept = std::fs::metadata(dir.join("file.reclaim-changed")).unwrap();
        assert_eq!(kept.permissions().mode() & 0o777, 0o600);
    }

    fn macl(path: &Path) -> Option<Vec<u8>> {
        let fd = sys::open(path, libc::O_RDONLY).unwrap();
        let mut value = vec![0u8; 1024];
        let n = unsafe {
            libc::fgetxattr(
                fd.as_raw_fd(),
                c"com.apple.macl".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        (n >= 0).then(|| value[..n as usize].to_vec())
    }

    /// A `com.apple.macl` value with one app grant.
    fn grant(app: u8) -> Vec<u8> {
        let mut v = vec![0u8; 72];
        v[..2].copy_from_slice(&[0x06, 0x00]);
        v[2..18].fill(app);
        v
    }

    #[test]
    fn the_app_grants_stay_with_the_name() {
        let dir = tempdir();
        let path = dir.join("file");
        std::fs::write(&path, b"old").unwrap();
        let fd = sys::open(&path, libc::O_RDONLY).unwrap();
        sys::set_xattr(fd.as_raw_fd(), c"com.apple.macl", &grant(1), 0, 0).unwrap();
        drop(fd);
        let d = dir.clone();
        let (replaced, _) = replace_with(&path, b"new", move || {
            let tmp = names(&d).into_iter().find(|n| n.starts_with(TMP_MARKER));
            let fd = sys::open(&d.join(tmp.unwrap()), libc::O_RDONLY).unwrap();
            sys::set_xattr(fd.as_raw_fd(), c"com.apple.macl", &grant(2), 0, 0).unwrap();
        });

        assert!(matches!(replaced, Replaced::Clean));
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(macl(&path), Some(grant(1)));
        assert_eq!(names(&dir), ["file"]);
    }

    #[test]
    fn keeps_a_changed_file_under_a_free_name() {
        let dir = tempdir();
        let path = dir.join("file");
        std::fs::write(&path, b"old").unwrap();
        std::fs::write(dir.join("file.reclaim-changed"), b"kept before").unwrap();
        let p = path.clone();
        let (replaced, _) = replace_with(&path, b"new", move || {
            std::fs::OpenOptions::new()
                .append(true)
                .open(p)
                .unwrap()
                .write_all(b" and appended")
                .unwrap();
        });

        let kept = dir.join("file.reclaim-changed.2");
        assert!(
            matches!(replaced, Replaced::KeptChanged(ref p) if *p == std::fs::canonicalize(&kept).unwrap())
        );
        assert_eq!(std::fs::read(&kept).unwrap(), b"old and appended");
        assert_eq!(
            std::fs::read(dir.join("file.reclaim-changed")).unwrap(),
            b"kept before"
        );
    }

    #[test]
    fn puts_the_file_back_if_another_file_takes_the_temporary_name() {
        let dir = tempdir();
        let path = dir.join("file");
        std::fs::write(&path, b"old").unwrap();
        let d = dir.clone();
        let dirs = scanned_dir(&path);
        let touched = TouchedDirs::new();
        let f = scanned(&path);
        let res = {
            let dir = DirFd::open(&dirs, 0, &touched).unwrap();
            let orig = dir.open_file(&f.name).unwrap();
            let (t, tfd) = Tmp::new_file(&dir).unwrap();
            std::fs::File::from(tfd).write_all(b"new").unwrap();
            BEFORE_SWAP.set(Some(Box::new(move || {
                let tmp = names(&d).into_iter().find(|n| n.starts_with(TMP_MARKER));
                std::fs::write(d.join("other"), b"other").unwrap();
                std::fs::rename(d.join("other"), d.join(tmp.unwrap())).unwrap();
            })));
            t.replace(&f.name, orig, (&f).into(), false)
        };

        assert!(res.is_err_and(|e| e.get_ref().is_some_and(|e| e.is::<Changed>())));
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert_eq!(names(&dir).len(), 2, "the other file is not removed");
    }

    #[test]
    fn refuses_a_file_mapped_for_writing() {
        let dir = tempdir();
        let path = dir.join("file");
        std::fs::write(&path, b"old").unwrap();
        let f = scanned(&path);
        let fd = sys::open(&path, libc::O_RDWR).unwrap();
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                3,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED);
        drop(fd);
        unsafe { map.cast::<u8>().write_volatile(b'O') };

        let dirs = scanned_dir(&path);
        let touched = TouchedDirs::new();
        let dir = DirFd::open(&dirs, 0, &touched).unwrap();
        let orig = dir.open_file(&f.name).unwrap();
        let (t, tfd) = Tmp::new_file(&dir).unwrap();
        std::fs::File::from(tfd).write_all(b"new").unwrap();
        let res = t.replace(&f.name, orig, (&f).into(), false);
        unsafe { libc::munmap(map, 3) };

        assert!(res.is_err_and(|e| e.get_ref().is_some_and(|e| e.is::<Unsupported>())));
        assert_eq!(std::fs::read(&path).unwrap(), b"Old");
    }
}
