//! Thin wrappers around the macOS APIs the tool needs: bulk directory listing with APFS clone
//! attributes, clones, extended attributes, times and flags, and libcompression.

use std::ffi::{CStr, CString, OsStr, c_void};
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub const ATTR_CMN_ERROR: u32 = 0x2000_0000;
pub const ATTR_CMNEXT_CLONE_REFCNT: u32 = 0x0000_1000;

pub const EF_MAY_SHARE_BLOCKS: u64 = 0x01;
pub const EF_NO_XATTRS: u64 = 0x02;
pub const EF_IS_SYNC_ROOT: u64 = 0x04;
pub const EF_IS_PURGEABLE: u64 = 0x08;
pub const EF_SHARES_ALL_BLOCKS: u64 = 0x40;

pub const SF_DATALESS: u32 = 0x4000_0000;

pub const VREG: u32 = 1;
pub const VDIR: u32 = 2;

pub fn cstr(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).expect("path contains a NUL byte")
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i64,
}

impl Timespec {
    fn to_libc(self) -> libc::timespec {
        libc::timespec {
            tv_sec: self.sec,
            tv_nsec: self.nsec,
        }
    }
}

/// Attributes of one directory entry, as returned by `getattrlistbulk`.
#[derive(Clone, Debug)]
pub struct EntryAttrs<'a> {
    pub name: &'a OsStr,
    pub error: u32,
    pub dev: u32,
    pub objtype: u32,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub uid: u32,
    pub flags: u32,
    /// Changes whenever the data changes. It is 0 while a process has the file mapped for writing.
    pub generation: u32,
    pub ino: u64,
    pub nlink: u32,
    pub size: u64,
    pub alloc: u64,
    pub clone_id: u64,
    pub ext_flags: u64,
}

const BULK_COMMON: u32 = libc::ATTR_CMN_RETURNED_ATTRS
    | ATTR_CMN_ERROR
    | libc::ATTR_CMN_NAME
    | libc::ATTR_CMN_DEVID
    | libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_MODTIME
    | libc::ATTR_CMN_CHGTIME
    | libc::ATTR_CMN_OWNERID
    | libc::ATTR_CMN_FLAGS
    | libc::ATTR_CMN_GEN_COUNT
    | libc::ATTR_CMN_FILEID;
// ATTR_FILE_TOTALSIZE would include the resource fork, so the size is the data fork's length.
const BULK_FILE: u32 =
    libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE | libc::ATTR_FILE_DATALENGTH;
// The private size and the clone reference count each make a directory listing about twice as
// slow, so they are only fetched per file where needed.
const BULK_EXT: u32 = libc::ATTR_CMNEXT_CLONEID | libc::ATTR_CMNEXT_EXT_FLAGS;

fn attrlist(common: u32, file: u32, ext: u32) -> libc::attrlist {
    libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: common,
        volattr: 0,
        dirattr: 0,
        fileattr: file,
        forkattr: ext,
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn u32(&mut self) -> u32 {
        let v = u32::from_le_bytes(self.buf[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        v
    }
    fn u64(&mut self) -> u64 {
        let v = u64::from_le_bytes(self.buf[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        v
    }
    fn timespec(&mut self) -> Timespec {
        Timespec {
            sec: self.u64() as i64,
            nsec: self.u64() as i64,
        }
    }
}

/// Lists a directory with `getattrlistbulk`, calling `f` for every entry.
pub fn list_dir(dirfd: RawFd, buf: &mut [u8], mut f: impl FnMut(EntryAttrs)) -> io::Result<()> {
    let mut al = attrlist(BULK_COMMON, BULK_FILE, BULK_EXT);
    let options = (libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_ATTR_CMN_EXTENDED) as u64;
    loop {
        let n = unsafe {
            libc::getattrlistbulk(
                dirfd,
                (&raw mut al).cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                options,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Ok(());
        }
        let mut start = 0usize;
        for _ in 0..n {
            let len = u32::from_le_bytes(buf[start..start + 4].try_into().unwrap()) as usize;
            let entry = &buf[start..start + len];
            // Directories come without the file attributes, despite FSOPT_PACK_INVAL_ATTRS.
            let returned_file = u32::from_le_bytes(entry[16..20].try_into().unwrap());
            let mut c = Cursor {
                buf: entry,
                pos: 4 + 20,
            };
            let error = c.u32();
            let name_ref_pos = c.pos;
            let name_off = c.u32() as i32;
            let name_len = c.u32() as usize;
            let name_start = (name_ref_pos as isize + name_off as isize) as usize;
            // The length includes the terminating NUL.
            let name =
                OsStr::from_bytes(&entry[name_start..name_start + name_len.saturating_sub(1)]);
            let dev = c.u32();
            let objtype = c.u32();
            let mtime = c.timespec();
            let ctime = c.timespec();
            let uid = c.u32();
            let flags = c.u32();
            let generation = c.u32();
            let ino = c.u64();
            let (nlink, alloc, size) = if returned_file != 0 {
                (c.u32(), c.u64(), c.u64())
            } else {
                (0, 0, 0)
            };
            let clone_id = c.u64();
            let ext_flags = c.u64();
            f(EntryAttrs {
                name,
                error,
                dev,
                objtype,
                mtime,
                ctime,
                uid,
                flags,
                generation,
                ino,
                nlink,
                size,
                alloc,
                clone_id,
                ext_flags,
            });
            start += len;
        }
    }
}

/// Attributes of a single file, including the two that are too slow to list for every file.
#[derive(Clone, Copy, Debug)]
pub struct FileAttrs {
    pub alloc: u64,
    /// Bytes that no other file (clone) shares.
    pub private_size: u64,
    /// The number of files that share all blocks with this one, including itself.
    pub clone_refcnt: u32,
}

fn file_attrs_list() -> libc::attrlist {
    attrlist(
        libc::ATTR_CMN_RETURNED_ATTRS,
        libc::ATTR_FILE_ALLOCSIZE,
        libc::ATTR_CMNEXT_PRIVATESIZE | ATTR_CMNEXT_CLONE_REFCNT,
    )
}

pub fn file_attrs(path: &Path) -> io::Result<FileAttrs> {
    let mut al = file_attrs_list();
    let mut buf = [0u8; 128];
    let options =
        libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_NOFOLLOW;
    let c = cstr(path);
    check(unsafe {
        libc::getattrlist(
            c.as_ptr(),
            (&raw mut al).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            options,
        )
    })?;
    parse_file_attrs(&buf)
}

/// Like [`file_attrs`], for an open file, which may already be unlinked.
pub fn fd_file_attrs(fd: RawFd) -> io::Result<FileAttrs> {
    let mut al = file_attrs_list();
    let mut buf = [0u8; 128];
    let options = libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_ATTR_CMN_EXTENDED;
    check(unsafe {
        libc::fgetattrlist(
            fd,
            (&raw mut al).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            options,
        )
    })?;
    parse_file_attrs(&buf)
}

/// The extended flags (`EF_*`) of `path`, without following a final symlink.
pub fn ext_flags(path: &Path) -> io::Result<u64> {
    let mut al = attrlist(
        libc::ATTR_CMN_RETURNED_ATTRS,
        0,
        libc::ATTR_CMNEXT_EXT_FLAGS,
    );
    let mut buf = [0u8; 64];
    let options =
        libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_NOFOLLOW;
    let c = cstr(path);
    check(unsafe {
        libc::getattrlist(
            c.as_ptr(),
            (&raw mut al).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            options,
        )
    })?;
    Ok(Cursor {
        buf: &buf,
        pos: 4 + 20,
    }
    .u64())
}

/// The generation count of the open file `fd`. Equal non-zero values mean identical data, and it
/// is 0 while a process has the file mapped for writing.
pub fn gen_count(fd: RawFd) -> io::Result<u32> {
    let mut al = attrlist(
        libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_GEN_COUNT,
        0,
        0,
    );
    let mut buf = [0u8; 64];
    let options = libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_ATTR_CMN_EXTENDED;
    check(unsafe {
        libc::fgetattrlist(
            fd,
            (&raw mut al).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            options,
        )
    })?;
    let mut c = Cursor { buf: &buf, pos: 4 };
    if c.u32() & libc::ATTR_CMN_GEN_COUNT == 0 {
        return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
    }
    c.pos = 4 + 20;
    Ok(c.u32())
}

fn parse_file_attrs(buf: &[u8]) -> io::Result<FileAttrs> {
    let mut c = Cursor { buf, pos: 4 + 20 };
    Ok(FileAttrs {
        alloc: c.u64(),
        private_size: c.u64(),
        clone_refcnt: c.u32(),
    })
}

pub fn open(path: &Path, flags: libc::c_int) -> io::Result<OwnedFd> {
    let c = cstr(path);
    let fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC, 0o600 as libc::c_uint) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// Times that `rename` over a file would otherwise change or lose.
#[derive(Clone, Copy, Debug)]
pub struct Times {
    pub crtime: Timespec,
    pub mtime: Timespec,
    pub atime: Timespec,
    pub addedtime: Timespec,
}

const TIMES_ATTRS: u32 = libc::ATTR_CMN_CRTIME
    | libc::ATTR_CMN_MODTIME
    | libc::ATTR_CMN_ACCTIME
    | libc::ATTR_CMN_ADDEDTIME;

pub fn get_times(fd: RawFd) -> io::Result<Times> {
    let mut al = attrlist(libc::ATTR_CMN_RETURNED_ATTRS | TIMES_ATTRS, 0, 0);
    let mut buf = [0u8; 128];
    check(unsafe {
        libc::fgetattrlist(
            fd,
            (&raw mut al).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::FSOPT_PACK_INVAL_ATTRS,
        )
    })?;
    let mut c = Cursor {
        buf: &buf,
        pos: 4 + 20,
    };
    Ok(Times {
        crtime: c.timespec(),
        mtime: c.timespec(),
        atime: c.timespec(),
        addedtime: c.timespec(),
    })
}

pub fn set_times(fd: RawFd, t: &Times) -> io::Result<()> {
    let mut al = attrlist(TIMES_ATTRS, 0, 0);
    let mut buf = [
        t.crtime.to_libc(),
        t.mtime.to_libc(),
        t.atime.to_libc(),
        t.addedtime.to_libc(),
    ];
    check(unsafe {
        libc::fsetattrlist(
            fd,
            (&raw mut al).cast(),
            buf.as_mut_ptr().cast(),
            size_of_val(&buf),
            0,
        )
    })
}

pub fn set_mtime_fd(fd: RawFd, mtime: Timespec) -> io::Result<()> {
    let mut al = attrlist(libc::ATTR_CMN_MODTIME, 0, 0);
    let mut buf = [mtime.to_libc()];
    check(unsafe {
        libc::fsetattrlist(
            fd,
            (&raw mut al).cast(),
            buf.as_mut_ptr().cast(),
            size_of_val(&buf),
            0,
        )
    })
}

pub fn mtime_of(st: &libc::stat) -> Timespec {
    Timespec {
        sec: st.st_mtime,
        nsec: st.st_mtime_nsec,
    }
}

const CLONE_NOFOLLOW: u32 = 0x1;
const CLONE_NOOWNERCOPY: u32 = 0x2;

pub fn openat(dirfd: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::openat(
            dirfd,
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// `lstat` of the entry `name` in the directory `dirfd`.
pub fn fstatat(dirfd: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstatat(dirfd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(st)
}

/// Clones the open file `src` to a new entry `name` in the directory `dirfd`.
pub fn fclonefileat(src: RawFd, dirfd: RawFd, name: &CStr) -> io::Result<()> {
    check(unsafe {
        libc::fclonefileat(
            src,
            dirfd,
            name.as_ptr(),
            CLONE_NOFOLLOW | CLONE_NOOWNERCOPY,
        )
    })
}

/// Atomically exchanges the entries `a` and `b` of the directory `dirfd`.
pub fn swap(dirfd: RawFd, a: &CStr, b: &CStr) -> io::Result<()> {
    check(unsafe { libc::renameatx_np(dirfd, a.as_ptr(), dirfd, b.as_ptr(), libc::RENAME_SWAP) })
}

/// Renames `from` to `to` in the directory `dirfd`, failing if `to` exists.
pub fn rename_excl(dirfd: RawFd, from: &CStr, to: &CStr) -> io::Result<()> {
    check(unsafe {
        libc::renameatx_np(dirfd, from.as_ptr(), dirfd, to.as_ptr(), libc::RENAME_EXCL)
    })
}

pub fn unlinkat(dirfd: RawFd, name: &CStr) -> io::Result<()> {
    check(unsafe { libc::unlinkat(dirfd, name.as_ptr(), 0) })
}

/// Hands the data written to `fd` to the drive, which may still reorder it with later writes.
pub fn fsync(fd: RawFd) -> io::Result<()> {
    check(unsafe { libc::fsync(fd) })
}

/// Makes sure that the data written to `fd` reaches the disk before any later write.
pub fn barrier_fsync(fd: RawFd) -> io::Result<()> {
    if unsafe { libc::fcntl(fd, libc::F_BARRIERFSYNC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn fchflags(fd: RawFd, flags: u32) -> io::Result<()> {
    check(unsafe { libc::fchflags(fd, flags) })
}

pub fn fchmod(fd: RawFd, mode: u32) -> io::Result<()> {
    check(unsafe { libc::fchmod(fd, (mode & 0o7777) as libc::mode_t) })
}

pub fn fchown(fd: RawFd, uid: u32, gid: u32) -> io::Result<()> {
    check(unsafe { libc::fchown(fd, uid, gid) })
}

pub fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}

pub fn ctime_of(st: &libc::stat) -> Timespec {
    Timespec {
        sec: st.st_ctime,
        nsec: st.st_ctime_nsec,
    }
}

unsafe extern "C" {
    fn acl_get_fd_np(fd: libc::c_int, ty: libc::c_int) -> *mut c_void;
    fn acl_free(obj: *mut c_void) -> libc::c_int;
}
const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;

/// Whether `fd` has an extended ACL. Fails if the ACL cannot be read, e.g. when an ACL denies
/// `readsecurity`, since a file with unknown protections cannot be replaced safely.
pub fn has_acl(fd: RawFd) -> io::Result<bool> {
    let acl = unsafe { acl_get_fd_np(fd, ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::ENOENT) {
            Ok(false)
        } else {
            Err(e)
        };
    }
    unsafe { acl_free(acl) };
    Ok(true)
}

/// Extended attributes that are visible without `XATTR_SHOWCOMPRESSION` and that the kernel does
/// not manage itself, sorted by name.
pub type Xattrs = Vec<(CString, Vec<u8>)>;

pub const XATTR_RESOURCE_FORK: &CStr = c"com.apple.ResourceFork";
pub const XATTR_DECMPFS: &CStr = c"com.apple.decmpfs";
/// The kernel tags files with the process that wrote them and refuses to remove the tag.
pub const XATTR_PROVENANCE: &CStr = c"com.apple.provenance";
/// The apps that may open a file (TCC). The kernel keeps them with the name, so a swap exchanges
/// them between the two files. The kernel also rewrites what processes write to them.
pub const XATTR_MACL: &CStr = c"com.apple.macl";

/// Whether the open file `fd` has the extended attribute `name`.
pub fn has_xattr(fd: RawFd, name: &CStr) -> bool {
    unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) >= 0 }
}

pub fn list_xattrs(fd: RawFd) -> io::Result<Xattrs> {
    let mut names = vec![0u8; 1024];
    let len = loop {
        let n = unsafe { libc::flistxattr(fd, names.as_mut_ptr().cast(), names.len(), 0) };
        if n >= 0 {
            break n as usize;
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ERANGE) {
            return Err(err);
        }
        let need = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0, 0) };
        if need < 0 {
            return Err(io::Error::last_os_error());
        }
        names.resize(need as usize + 1, 0);
    };
    let mut out = Vec::new();
    for name in names[..len].split(|&b| b == 0).filter(|n| !n.is_empty()) {
        let name = CString::new(name).unwrap();
        if name.as_c_str() == XATTR_PROVENANCE || name.as_c_str() == XATTR_MACL {
            continue;
        }
        let size = unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut value = vec![0u8; size as usize];
        let n = unsafe {
            libc::fgetxattr(
                fd,
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        value.truncate(n as usize);
        out.push((name, value));
    }
    out.sort();
    Ok(out)
}

pub fn set_xattr(
    fd: RawFd,
    name: &CStr,
    value: &[u8],
    position: u32,
    options: libc::c_int,
) -> io::Result<()> {
    check(unsafe {
        libc::fsetxattr(
            fd,
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            position,
            options,
        )
    })
}

pub fn remove_xattr(fd: RawFd, name: &CStr) -> io::Result<()> {
    check(unsafe { libc::fremovexattr(fd, name.as_ptr(), 0) })
}

/// Makes the visible extended attributes of `fd` equal to `want`.
pub fn sync_xattrs(fd: RawFd, want: &Xattrs) -> io::Result<()> {
    let have = list_xattrs(fd)?;
    for (name, _) in &have {
        if !want.iter().any(|(n, _)| n == name) {
            remove_xattr(fd, name)?;
        }
    }
    for (name, value) in want {
        if !have.iter().any(|(n, v)| n == name && v == value) {
            set_xattr(fd, name, value, 0, 0)?;
        }
    }
    Ok(())
}

pub fn free_space(path: &Path) -> io::Result<u64> {
    let c = cstr(path);
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    check(unsafe { libc::statfs(c.as_ptr(), &mut st) })?;
    Ok(st.f_bavail * st.f_bsize as u64)
}

#[link(name = "compression")]
unsafe extern "C" {
    fn compression_encode_scratch_buffer_size(alg: u32) -> usize;
    fn compression_encode_buffer(
        dst: *mut u8,
        dst_size: usize,
        src: *const u8,
        src_size: usize,
        scratch: *mut c_void,
        alg: u32,
    ) -> usize;
}

pub fn encode_scratch_size(alg: u32) -> usize {
    unsafe { compression_encode_scratch_buffer_size(alg) }
}

/// Compresses `src` into `dst` and returns the compressed length, or 0 if it does not fit.
pub fn encode(alg: u32, dst: &mut [u8], src: &[u8], scratch: &mut [u8]) -> usize {
    unsafe {
        compression_encode_buffer(
            dst.as_mut_ptr(),
            dst.len(),
            src.as_ptr(),
            src.len(),
            scratch.as_mut_ptr().cast(),
            alg,
        )
    }
}

#[repr(C)]
struct ProcFdInfo {
    fd: i32,
    fdtype: u32,
}

#[repr(C)]
struct ProcFileInfo {
    openflags: u32,
    status: u32,
    offset: i64,
    ty: i32,
    guardflags: u32,
}

#[repr(C)]
struct VnodeFdInfo {
    pfi: ProcFileInfo,
    pvi: libc::vnode_info,
}

const PROC_PIDFDVNODEINFO: libc::c_int = 1;
const FWRITE: u32 = 0x2;

pub struct OpenForWriting {
    /// `(dev, ino)` of the files that processes have open for writing.
    pub files: std::collections::HashSet<(u32, u64)>,
    /// Processes of this user whose open files could not be listed.
    pub uninspected: usize,
}

/// Lists the files that processes this user can inspect (those of the same user, without root)
/// have open for writing.
pub fn files_open_for_writing() -> OpenForWriting {
    let mut out = std::collections::HashSet::new();
    let mut uninspected = 0;
    let uid = unsafe { libc::geteuid() };
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return OpenForWriting {
            files: out,
            uninspected: 1,
        };
    }
    let mut pids = vec![0i32; n as usize + 64];
    let n = unsafe {
        libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as libc::c_int)
    };
    let mut fds: Vec<ProcFdInfo> = Vec::new();
    for &pid in &pids[..n.max(0) as usize] {
        let mut failed = || {
            if process_uid(pid) == Some(uid) {
                uninspected += 1;
            }
        };
        let size =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if size <= 0 {
            failed();
            continue;
        }
        fds.clear();
        fds.reserve(size as usize / size_of::<ProcFdInfo>() + 32);
        let size = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                (fds.capacity() * size_of::<ProcFdInfo>()) as libc::c_int,
            )
        };
        if size <= 0 {
            failed();
            continue;
        }
        unsafe { fds.set_len(size as usize / size_of::<ProcFdInfo>()) };
        for fd in &fds {
            if fd.fdtype != libc::PROX_FDTYPE_VNODE as u32 {
                continue;
            }
            let mut info: VnodeFdInfo = unsafe { std::mem::zeroed() };
            let got = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.fd,
                    PROC_PIDFDVNODEINFO,
                    (&raw mut info).cast(),
                    size_of::<VnodeFdInfo>() as libc::c_int,
                )
            };
            if got as usize != size_of::<VnodeFdInfo>() {
                // The descriptor may have been closed since the listing.
                continue;
            }
            if info.pfi.openflags & FWRITE != 0 {
                out.insert((info.pvi.vi_stat.vst_dev, info.pvi.vi_stat.vst_ino));
            }
        }
    }
    OpenForWriting {
        files: out,
        uninspected,
    }
}

fn process_uid(pid: libc::pid_t) -> Option<u32> {
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    (got == size).then_some(info.pbsi_uid)
}
