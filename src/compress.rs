//! APFS transparent compression (decmpfs): writes a compressed copy of a file and swaps it in.
//!
//! The data is split into 64 KiB blocks that are compressed independently. A file whose single
//! compressed block is small enough lives entirely in the `com.apple.decmpfs` xattr; otherwise
//! the blocks go into the resource fork, preceded by a table of `u32` block offsets.

use crate::replace::{self, DirFd, Identity, Meta, Replaced, Tmp, unsupported};
use crate::scan::File;
use crate::sys;
use rayon::prelude::*;
use std::cell::RefCell;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};

pub const BLOCK: usize = 64 * 1024;
const MAX_INLINE: usize = 3802 - 16;
/// APFS keeps a decmpfs xattr in the inode only up to about this size. A larger one takes a block
/// of its own, as a resource fork would.
const EMBEDDED_XATTR: usize = 120;
const CHUNK_BLOCKS: usize = 64;
const APFS_BLOCK: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Algorithm {
    /// LZFSE for files up to 32 KiB, LZBITMAP for larger ones.
    Auto,
    /// Fastest to compress and decompress, but weak on small inputs.
    Lzbitmap,
    /// About 10% smaller than LZBITMAP and half as fast to compress.
    Lzfse,
    /// Like LZBITMAP in size, but slower.
    Lzvn,
}

/// Files up to this size have few blocks, so the better ratio of LZFSE more often saves a whole
/// block, while the compression time is small next to the cost of replacing the file.
const AUTO_LZFSE_MAX: u64 = 32 * 1024;

impl Algorithm {
    fn for_size(self, size: u64) -> Algorithm {
        match self {
            Algorithm::Auto if size <= AUTO_LZFSE_MAX => Algorithm::Lzfse,
            Algorithm::Auto => Algorithm::Lzbitmap,
            a => a,
        }
    }

    fn libcompression_id(self) -> u32 {
        match self {
            Algorithm::Auto => unreachable!(),
            Algorithm::Lzbitmap => 0x702,
            Algorithm::Lzfse => 0x801,
            // Not in the public header, but libcompression implements LZVN under this id.
            Algorithm::Lzvn => 0x900,
        }
    }

    /// decmpfs compression types for data in the xattr and in the resource fork.
    fn decmpfs_types(self) -> (u32, u32) {
        match self {
            Algorithm::Auto => unreachable!(),
            Algorithm::Lzvn => (7, 8),
            Algorithm::Lzfse => (11, 12),
            Algorithm::Lzbitmap => (13, 14),
        }
    }

    /// Byte that marks a block stored uncompressed, for formats that have one.
    fn raw_prefix(self) -> Option<u8> {
        match self {
            Algorithm::Lzvn => Some(0x06),
            Algorithm::Auto | Algorithm::Lzfse | Algorithm::Lzbitmap => None,
        }
    }
}

thread_local! {
    static SCRATCH: RefCell<Option<(u32, Vec<u8>)>> = const { RefCell::new(None) };
}

fn compress_block(alg: Algorithm, src: &[u8]) -> Option<Vec<u8>> {
    let id = alg.libcompression_id();
    SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        if s.as_ref().is_none_or(|(a, _)| *a != id) {
            *s = Some((id, vec![0u8; sys::encode_scratch_size(id).max(1)]));
        }
        let scratch = &mut s.as_mut().unwrap().1;
        let mut dst = vec![0u8; src.len() + 1024];
        let n = sys::encode(id, &mut dst, src, scratch);
        if n > 0 && n < src.len() + 1 {
            dst.truncate(n);
            return Some(dst);
        }
        match alg.raw_prefix() {
            Some(p) => {
                let mut raw = Vec::with_capacity(src.len() + 1);
                raw.push(p);
                raw.extend_from_slice(src);
                Some(raw)
            }
            None if n > 0 => {
                dst.truncate(n);
                Some(dst)
            }
            None => None,
        }
    })
}

pub enum Outcome<'d> {
    /// `prepared` is `None` in a dry run.
    Compressed {
        prepared: Option<Prepared<'d>>,
        new_alloc: u64,
    },
    NotWorthIt,
}

/// A compressed copy of a file with the file's metadata, not yet in place of the file.
pub struct Prepared<'d> {
    tmp: Tmp<'d>,
    /// The compressed copy, under a temporary name only this process uses.
    pub fd: OwnedFd,
    /// The compressed copy's identity once it was complete.
    pub id: Identity,
    orig: OwnedFd,
}

impl Prepared<'_> {
    /// Puts the compressed copy in place of `f`. Returns how it went and, if `keep_open`, the
    /// replaced file, still open.
    pub fn commit(self, f: &File, keep_open: bool) -> io::Result<(Replaced, Option<OwnedFd>)> {
        drop(self.fd);
        self.tmp.replace(&f.name, self.orig, f.into(), keep_open)
    }
}

pub struct Settings {
    pub algorithm: Algorithm,
    pub dry_run: bool,
    /// Compression must save at least this share of the allocated size.
    pub min_saving: f64,
    /// Order the compressed data before the swap even on power loss, at about 1 ms per file.
    pub durable: bool,
}

fn pread_full(fd: &OwnedFd, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        let n = unsafe {
            libc::pread(
                fd.as_raw_fd(),
                buf[done..].as_mut_ptr().cast(),
                buf.len() - done,
                (offset + done as u64) as i64,
            )
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
        done += n as usize;
    }
    Ok(done)
}

fn no_cache(fd: &OwnedFd) {
    unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_NOCACHE, 1) };
}

pub fn allocated(bytes: u64) -> u64 {
    bytes.div_ceil(APFS_BLOCK) * APFS_BLOCK
}

/// Whether three sample blocks spread over a large file show that compressing it is not worth it.
/// Returns false for files of fewer than 16 blocks without reading them.
fn sample_says_incompressible(
    fd: &OwnedFd,
    s: &Settings,
    alg: Algorithm,
    nblocks: u64,
) -> io::Result<bool> {
    if nblocks < 16 {
        return Ok(false);
    }
    let picks = [0, nblocks / 2, nblocks - 2];
    let blocks: Vec<Vec<u8>> = picks
        .iter()
        .map(|&b| {
            let mut buf = vec![0u8; BLOCK];
            let n = pread_full(fd, &mut buf, b * BLOCK as u64)?;
            buf.truncate(n);
            Ok(buf)
        })
        .collect::<io::Result<_>>()?;
    let raw: usize = blocks.iter().map(Vec::len).sum();
    let compressed: usize = blocks
        .par_iter()
        .map(|b| compress_block(alg, b).map_or(b.len(), |c| c.len()))
        .sum();
    Ok(compressed as f64 > raw as f64 * (1.0 - s.min_saving))
}

/// Makes a compressed copy of the file `f` in `dir`, if that is worth it.
pub fn prepare<'d>(dir: &'d DirFd<'d>, f: &File, s: &Settings) -> io::Result<Outcome<'d>> {
    let fd = dir.open_file(&f.name)?;
    replace::unchanged_fd(&fd, f.into())?;
    no_cache(&fd);
    let meta = Meta::read(&fd, f)?;
    let nblocks = f.size.div_ceil(BLOCK as u64);
    if f.size > u32::MAX as u64 / 2 {
        return Err(unsupported("too large"));
    }
    let alg = s.algorithm.for_size(f.size);
    if sample_says_incompressible(&fd, s, alg, nblocks)? {
        return Ok(Outcome::NotWorthIt);
    }
    let (inline_type, rfork_type) = alg.decmpfs_types();
    let header_len = 4 * (nblocks + 1);
    let max_compressed = (f.alloc as f64 * (1.0 - s.min_saving)) as u64;

    let mut tmp: Option<(Tmp, OwnedFd)> = None;
    let mut offsets: Vec<u32> = Vec::with_capacity(nblocks as usize + 1);
    let mut pending: Vec<u8> = Vec::new();
    let mut written_rfork = header_len;
    let mut total_compressed = 0u64;
    let mut single_block: Option<Vec<u8>> = None;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; BLOCK * CHUNK_BLOCKS];
    let mut offset = 0u64;
    while offset < f.size {
        let want = ((f.size - offset) as usize).min(buf.len());
        let n = pread_full(&fd, &mut buf[..want], offset)?;
        if n != want {
            return Err(replace::changed());
        }
        hasher.update(&buf[..n]);
        let blocks: Vec<Vec<u8>> = buf[..n]
            .par_chunks(BLOCK)
            .map(|b| compress_block(alg, b))
            .collect::<Option<_>>()
            .ok_or_else(|| unsupported("block did not compress"))?;
        offset += n as u64;
        for b in blocks {
            total_compressed += b.len() as u64;
            offsets.push((header_len + total_compressed - b.len() as u64) as u32);
            if nblocks == 1 {
                single_block = Some(b);
            } else {
                pending.extend_from_slice(&b);
            }
        }
        let ratio_so_far = total_compressed as f64 / offset as f64;
        if total_compressed > max_compressed
            || (offset >= 2 * (BLOCK * CHUNK_BLOCKS) as u64 && ratio_so_far > 1.0 - s.min_saving)
        {
            return Ok(Outcome::NotWorthIt);
        }
        if !s.dry_run && pending.len() >= 8 << 20 {
            if tmp.is_none() {
                tmp = Some(create_tmp(dir)?);
            }
            let tfd = &tmp.as_ref().unwrap().1;
            sys::set_xattr(
                tfd.as_raw_fd(),
                sys::XATTR_RESOURCE_FORK,
                &pending,
                written_rfork as u32,
                libc::XATTR_SHOWCOMPRESSION,
            )?;
            written_rfork += pending.len() as u64;
            pending.clear();
        }
    }
    offsets.push((header_len + total_compressed) as u32);

    let inline = single_block.as_ref().is_some_and(|b| b.len() <= MAX_INLINE);
    let new_alloc = match &single_block {
        Some(b) if inline && 16 + b.len() <= EMBEDDED_XATTR => 0,
        _ if inline => APFS_BLOCK,
        _ => allocated(header_len + total_compressed),
    };
    if new_alloc + APFS_BLOCK > f.alloc || new_alloc > max_compressed {
        return Ok(Outcome::NotWorthIt);
    }
    if s.dry_run {
        return Ok(Outcome::Compressed {
            prepared: None,
            new_alloc,
        });
    }

    let (tmp, tfd) = match tmp {
        Some(t) => t,
        None => create_tmp(dir)?,
    };
    let raw = tfd.as_raw_fd();
    let mut decmpfs = Vec::with_capacity(16 + MAX_INLINE);
    decmpfs.extend_from_slice(b"fpmc");
    decmpfs.extend_from_slice(&(if inline { inline_type } else { rfork_type }).to_le_bytes());
    decmpfs.extend_from_slice(&f.size.to_le_bytes());
    if inline {
        decmpfs.extend_from_slice(single_block.as_ref().unwrap());
    } else {
        if let Some(b) = single_block {
            pending = b;
        }
        if !pending.is_empty() {
            sys::set_xattr(
                raw,
                sys::XATTR_RESOURCE_FORK,
                &pending,
                written_rfork as u32,
                libc::XATTR_SHOWCOMPRESSION,
            )?;
        }
        let table: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
        sys::set_xattr(
            raw,
            sys::XATTR_RESOURCE_FORK,
            &table,
            0,
            libc::XATTR_SHOWCOMPRESSION,
        )?;
    }
    sys::set_xattr(
        raw,
        sys::XATTR_DECMPFS,
        &decmpfs,
        0,
        libc::XATTR_SHOWCOMPRESSION,
    )?;
    sys::fchflags(raw, libc::UF_COMPRESSED)?;

    // Read the new file back through the kernel's decompression. An unsupported or malformed
    // compression type would silently read as zeros or as an empty file.
    let mut check = blake3::Hasher::new();
    let mut offset = 0u64;
    loop {
        let n = pread_full(&tfd, &mut buf, offset)?;
        if n == 0 {
            break;
        }
        check.update(&buf[..n]);
        offset += n as u64;
    }
    if offset != f.size || check.finalize() != hasher.finalize() {
        return Err(io::Error::other(
            "compressed copy does not read back identically",
        ));
    }

    meta.apply(&tfd, true)?;
    // Otherwise the data read may mix versions of the file.
    replace::unchanged_fd(&fd, f.into())?;
    // The compressed data must reach the disk before the copy replaces the original. Without a
    // barrier, the drive may still write it after the swap, which only matters on power loss.
    if s.durable {
        sys::barrier_fsync(raw)?;
    } else {
        sys::fsync(raw)?;
    }
    let id = Identity::of(&tfd)?;
    let new_alloc = sys::fd_file_attrs(raw).map_or(new_alloc, |a| a.alloc);
    Ok(Outcome::Compressed {
        prepared: Some(Prepared {
            tmp,
            fd: tfd,
            id,
            orig: fd,
        }),
        new_alloc,
    })
}

fn create_tmp<'d>(dir: &'d DirFd<'d>) -> io::Result<(Tmp<'d>, OwnedFd)> {
    let (tmp, fd) = Tmp::new_file(dir)?;
    no_cache(&fd);
    Ok((tmp, fd))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::replace::TouchedDirs;
    use crate::scan::{Dir, File};
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    pub(crate) fn text(len: usize, seed: u64) -> Vec<u8> {
        let words: [&[u8]; 6] = [
            b"alpha ",
            b"beta ",
            b"gamma ",
            b"return ",
            b"import os\n",
            b"x = 1\n",
        ];
        let mut x = seed | 1;
        let mut out = Vec::with_capacity(len + 16);
        while out.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.extend_from_slice(words[(x % 6) as usize]);
        }
        out.truncate(len);
        out
    }

    fn random(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    pub(crate) fn scanned(path: &Path) -> File {
        // `fstat` works where an ACL denies `lstat`.
        let fd = sys::open(path, libc::O_RDONLY | libc::O_NOFOLLOW).unwrap();
        let st = sys::fstat(fd.as_raw_fd()).unwrap();
        File {
            dir: 0,
            name: path.file_name().unwrap().into(),
            dev: st.st_dev as u32,
            ino: st.st_ino,
            size: st.st_size as u64,
            alloc: st.st_blocks as u64 * 512,
            clone_id: 0,
            ext_flags: 0,
            flags: 0,
            ctime: sys::ctime_of(&st),
            mtime: sys::mtime_of(&st),
            generation: sys::gen_count(fd.as_raw_fd()).unwrap(),
        }
    }

    /// The directory of `path` as the scan would see it.
    pub(crate) fn scanned_dir(path: &Path) -> Vec<Dir> {
        let dir = std::fs::canonicalize(path.parent().unwrap()).unwrap();
        let m = std::fs::metadata(&dir).unwrap();
        vec![Dir {
            path: dir,
            dev: m.dev() as u32,
            ino: m.ino(),
            mtime: sys::Timespec {
                sec: m.mtime(),
                nsec: m.mtime_nsec(),
            },
        }]
    }

    /// Compresses the file at `path` in place; returns whether it was worth it.
    fn compress_file(path: &Path, s: &Settings) -> io::Result<bool> {
        let dirs = scanned_dir(path);
        let touched = TouchedDirs::new();
        let dir = DirFd::open(&dirs, 0, &touched)?;
        let f = scanned(path);
        match prepare(&dir, &f, s)? {
            Outcome::Compressed {
                prepared: Some(p), ..
            } => {
                let (replaced, _) = p.commit(&f, false)?;
                assert!(matches!(replaced, Replaced::Clean));
                Ok(true)
            }
            Outcome::Compressed { prepared: None, .. } => Ok(true),
            Outcome::NotWorthIt => Ok(false),
        }
    }

    const ALL: Settings = Settings {
        algorithm: Algorithm::Auto,
        dry_run: false,
        min_saving: 0.0,
        durable: false,
    };

    fn entries(dir: &Path) -> Vec<PathBuf> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn an_acl_that_cannot_be_read_is_kept() {
        let dir = tempdir();
        let path = dir.join("protected");
        std::fs::write(&path, text(100_000, 1)).unwrap();
        let id = std::process::Command::new("id")
            .arg("-un")
            .output()
            .unwrap();
        let user = String::from_utf8(id.stdout).unwrap();
        let user = user.trim();
        let chmod = std::process::Command::new("chmod")
            .args(["+a", &format!("user:{user} deny readsecurity")])
            .arg(&path)
            .status()
            .unwrap();
        assert!(chmod.success());

        assert!(compress_file(&path, &ALL).is_err());
        let fd = sys::open(&path, libc::O_RDONLY).unwrap();
        assert_eq!(
            sys::has_acl(fd.as_raw_fd()).unwrap_err().raw_os_error(),
            Some(libc::EACCES),
            "the ACL is still there"
        );
        assert_eq!(entries(&dir), vec![path]);
    }

    #[test]
    fn files_with_the_names_of_temporary_files_are_kept() {
        let dir = tempdir();
        let path = dir.join("file");
        std::fs::write(&path, text(100_000, 2)).unwrap();
        let next = crate::replace::tests::next_tmp_number();
        let mut existing: Vec<PathBuf> = (next..next + 20)
            .map(|n| {
                let p = dir.join(format!(
                    "{}{}.{n}",
                    crate::replace::TMP_MARKER,
                    std::process::id()
                ));
                std::fs::write(&p, b"precious").unwrap();
                p
            })
            .collect();

        assert!(compress_file(&path, &ALL).unwrap());
        for p in &existing {
            assert_eq!(std::fs::read(p).unwrap(), b"precious");
        }
        existing.push(path);
        existing.sort();
        assert_eq!(entries(&dir), existing, "no temporary file is left behind");
    }

    #[test]
    fn compressed_files_read_back_identically() {
        let dir = tempdir();
        let sizes = [5000, 65535, 65536, 65537, 3 * 65536, (1 << 20) + 1];
        for alg in [
            Algorithm::Lzbitmap,
            Algorithm::Lzfse,
            Algorithm::Lzvn,
            Algorithm::Auto,
        ] {
            for &size in &sizes {
                // Mostly text with one incompressible block in the middle.
                let mut data = text(size, size as u64);
                if size > 2 * BLOCK {
                    data[BLOCK..2 * BLOCK].copy_from_slice(&random(BLOCK, 7));
                }
                let path = dir.join(format!("{alg:?}-{size}"));
                std::fs::write(&path, &data).unwrap();
                let settings = Settings {
                    algorithm: alg,
                    dry_run: false,
                    min_saving: 0.0,
                    durable: true,
                };
                assert!(compress_file(&path, &settings).unwrap(), "{alg:?} {size}");
                let m = std::fs::metadata(&path).unwrap();
                assert_ne!(
                    std::os::macos::fs::MetadataExt::st_flags(&m) & libc::UF_COMPRESSED,
                    0
                );
                assert_eq!(std::fs::read(&path).unwrap(), data, "{alg:?} {size}");
            }
        }
    }

    #[test]
    fn random_data_is_left_alone() {
        let dir = tempdir();
        let path = dir.join("random");
        let data = random(3 << 20, 3);
        std::fs::write(&path, &data).unwrap();
        let settings = Settings {
            algorithm: Algorithm::Auto,
            dry_run: false,
            min_saving: 0.1,
            durable: false,
        };
        assert!(!compress_file(&path, &settings).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temporary file is left behind"
        );
    }

    pub(crate) fn tempdir() -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "reclaim-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
