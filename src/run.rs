//! Plans what happens to each file and carries out the plan. Identical files become clones of one
//! copy, and files whose blocks no other file shares are compressed.

use crate::cache::Incompressible;
use crate::compress::{self, Outcome};
use crate::dedupe::{self, ContentGroup, Family};
use crate::replace::{self, Changed, DirFd, Identity, Meta, Replaced, TouchedDirs, Unsupported};
use crate::scan::{File, Scan};
use crate::{interrupted, size, sys};
use rayon::prelude::*;
use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The skip reason for clones that share their blocks with files outside the scanned paths.
pub const SHARED_OUTSIDE: &str = "files outside the given paths share its blocks";

#[derive(Default)]
pub struct Stats {
    pub cloned_files: AtomicU64,
    pub cloned_bytes: AtomicU64,
    pub dedupe_freed: AtomicU64,
    pub compressed_files: AtomicU64,
    pub compressed_before: AtomicU64,
    pub compressed_after: AtomicU64,
    pub incompressible_files: AtomicU64,
    pub incompressible_bytes: AtomicU64,
    pub units_done: AtomicU64,
    pub skipped: Mutex<HashMap<String, u64>>,
    /// Replaced files that changed between the last check and the swap, kept next to the new file.
    pub kept_changed: AtomicU64,
    /// Bytes of replaced files that stay allocated because files outside the scanned paths share them.
    pub still_shared: AtomicU64,
    pub still_shared_files: AtomicU64,
    /// Groups of identical files of which only some could be replaced, so that the blocks of the
    /// others stay allocated besides the compressed copy.
    pub partial_groups: AtomicU64,
    /// What the plan read to find identical files.
    pub hashed: dedupe::Hashed,
}

impl Stats {
    /// Bytes freed so far, computed from the run's own counts without querying the file system.
    pub fn freed(&self) -> u64 {
        (self.dedupe_freed.load(Ordering::Relaxed) + self.compressed_before.load(Ordering::Relaxed))
            .saturating_sub(
                self.compressed_after.load(Ordering::Relaxed)
                    + self.still_shared.load(Ordering::Relaxed),
            )
    }
}

pub struct Options {
    pub dry_run: bool,
    pub dedupe: bool,
    pub compress: bool,
    pub min_size: u64,
    pub verbose: bool,
    pub compression: compress::Settings,
}

pub struct Runner<'a> {
    scan: &'a Scan,
    opts: &'a Options,
    pub stats: Stats,
    touched: TouchedDirs,
    incompressible: Incompressible,
}

/// The work of a run: groups of files with the same content, and single files to compress.
pub struct Plan {
    pub groups: Vec<ContentGroup>,
    /// Files to compress, with the bytes that only they use.
    pub singles: Vec<(usize, u64)>,
    pub duplicates: usize,
}

impl Plan {
    pub fn units(&self) -> usize {
        self.groups.len() + self.singles.len()
    }
}

impl<'a> Runner<'a> {
    pub fn new(scan: &'a Scan, opts: &'a Options, incompressible: Incompressible) -> Self {
        Runner {
            scan,
            opts,
            stats: Stats::default(),
            touched: TouchedDirs::new(),
            incompressible,
        }
    }

    fn file(&self, i: usize) -> &File {
        &self.scan.files[i]
    }

    fn dir(&self, f: &File) -> io::Result<DirFd<'_>> {
        DirFd::open(&self.scan.dirs, f.dir, &self.touched)
    }

    fn skip(&self, what: &str, path: &Path, e: &io::Error) {
        let reason = if let Some(r) = e.get_ref().and_then(|r| r.downcast_ref::<Unsupported>()) {
            r.0.to_string()
        } else if e.get_ref().is_some_and(|r| r.is::<Changed>()) {
            "changed during the run".to_string()
        } else {
            format!("{what} failed: {}", e.kind())
        };
        if self.opts.verbose {
            eprintln!("{what} {}: {e}", path.display());
        }
        *self
            .stats
            .skipped
            .lock()
            .unwrap()
            .entry(reason)
            .or_default() += 1;
    }

    fn report(&self, f: &File, replaced: Replaced) {
        if let Replaced::KeptChanged(kept) = replaced {
            self.stats.kept_changed.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "warning: {} changed while it was replaced; the replaced file is kept as {}",
                self.scan.path(f).display(),
                kept.display()
            );
        }
    }

    /// The plan expected `fd`, as the last file that uses the `owned` bytes, to free them. Returns
    /// how many of them other files still use. The clone reference count only counts full clones,
    /// so only now does the private size of `fd` show files outside the scanned paths that share
    /// some of the blocks. Returns 0 if this cannot be determined.
    fn shared_elsewhere(&self, fd: &OwnedFd, owned: u64) -> u64 {
        let attrs = || sys::fd_file_attrs(fd.as_raw_fd());
        let Ok(mut a) = attrs() else {
            return 0;
        };
        // A full clone may be a removed file that another process still had open. APFS frees its
        // blocks only some time after it is closed.
        for _ in 0..10 {
            if a.clone_refcnt <= 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            let Ok(now) = attrs() else {
                return 0;
            };
            a = now;
        }
        if a.clone_refcnt > 1 {
            return 0;
        }
        // The private size only counts the data fork, which is empty for a compressed file. The
        // allocation also includes the resource fork, which holds the compressed data.
        if sys::fstat(fd.as_raw_fd()).is_ok_and(|st| st.st_flags & libc::UF_COMPRESSED != 0) {
            return 0;
        }
        owned.saturating_sub(a.private_size)
    }

    /// Checks that `old`, the last open reference to the blocks of the replaced file `f`, frees
    /// the `owned` bytes that the plan expected.
    fn check_released(&self, f: &File, old: Option<OwnedFd>, owned: u64) {
        let Some(old) = old else {
            return;
        };
        let shared = self.shared_elsewhere(&old, owned);
        if shared == 0 {
            return;
        }
        self.stats.still_shared.fetch_add(shared, Ordering::Relaxed);
        if shared * 10 > owned {
            self.stats
                .still_shared_files
                .fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "warning: {} of the replaced {} stay allocated, because files outside the given paths share them",
                size(shared),
                self.scan.path(f).display()
            );
        }
    }

    /// The bytes only file `i` uses, if that is (nearly) all of its allocation.
    fn unshared_bytes(&self, i: usize) -> Option<u64> {
        let f = self.file(i);
        if !f.may_share_blocks() {
            return Some(f.alloc);
        }
        if f.is_pure_clone() {
            return None;
        }
        let private = sys::file_attrs(&self.scan.path(f)).ok()?.private_size;
        (private * 10 >= f.alloc * 9).then_some(private)
    }

    /// The number of files that share all blocks with the pure clone `i`, including itself.
    fn clone_refcnt(&self, i: usize) -> u32 {
        sys::file_attrs(&self.scan.path(self.file(i))).map_or(0, |a| a.clone_refcnt)
    }

    /// Bytes that are freed once no file of the family uses its blocks any more, or `None` if the
    /// APFS clone attributes show that files outside the scanned paths use some of them too.
    fn owned_bytes(&self, fam: &Family) -> Option<u64> {
        let f = self.file(fam[0]);
        if f.is_pure_clone() {
            (self.clone_refcnt(fam[0]) as usize == fam.len()).then_some(f.alloc)
        } else {
            self.unshared_bytes(fam[0])
        }
    }

    fn known_incompressible(&self, fam: &Family) -> bool {
        fam.iter()
            .any(|&i| self.incompressible.contains(self.file(i)))
    }

    pub fn plan(&self) -> Plan {
        let mut groups = if self.opts.dedupe {
            dedupe::content_groups(self.scan, self.opts.min_size, &self.stats.hashed)
        } else {
            Vec::new()
        };
        let duplicates = groups.iter().flatten().map(Vec::len).sum();
        let mut in_group = vec![false; self.scan.files.len()];
        for &i in groups.iter().flatten().flatten() {
            in_group[i] = true;
        }
        if !self.opts.compress {
            return Plan {
                groups,
                singles: Vec::new(),
                duplicates,
            };
        }
        // Pure clones that no other content matched are worth compressing as a whole when no file
        // outside the scanned paths shares their blocks.
        let mut families: HashMap<u64, Family> = HashMap::new();
        let mut maybe_singles = Vec::new();
        for (i, f) in self.scan.files.iter().enumerate() {
            if in_group[i] || f.is_compressed() {
                continue;
            }
            if f.is_pure_clone() {
                families.entry(f.clone_id).or_default().push(i);
            } else {
                maybe_singles.push(i);
            }
        }
        let families: Vec<Family> = families.into_values().collect();
        groups.par_extend(
            families
                .into_par_iter()
                .filter(|fam| !self.known_incompressible(fam) && self.owned_bytes(fam).is_some())
                .map(|fam| vec![fam]),
        );
        let singles = maybe_singles
            .into_par_iter()
            .filter(|&i| !self.incompressible.contains(self.file(i)))
            .filter_map(|i| self.unshared_bytes(i).map(|owned| (i, owned)))
            .collect();
        Plan {
            groups,
            singles,
            duplicates,
        }
    }

    pub fn execute(&self, plan: &Plan) {
        plan.groups.par_iter().for_each(|g| {
            if !interrupted() {
                self.run_group(g);
            }
            self.stats.units_done.fetch_add(1, Ordering::Relaxed);
        });
        plan.singles.par_iter().for_each(|&(i, owned)| {
            if !interrupted() {
                self.compress_single(i, owned);
            }
            self.stats.units_done.fetch_add(1, Ordering::Relaxed);
        });
    }

    /// Restores directory modification times and saves the cache.
    pub fn finish(self) -> Stats {
        self.touched.restore(&self.scan.dirs);
        if !self.opts.dry_run
            && let Err(e) = self.incompressible.save()
        {
            eprintln!("could not save the list of incompressible files: {e}");
        }
        self.stats
    }

    fn count_compressed(&self, before: u64, after: u64) {
        self.stats.compressed_files.fetch_add(1, Ordering::Relaxed);
        self.stats
            .compressed_before
            .fetch_add(before, Ordering::Relaxed);
        self.stats
            .compressed_after
            .fetch_add(after, Ordering::Relaxed);
    }

    fn not_worth_it(&self, files: impl Iterator<Item = usize>, alloc: u64) {
        files.for_each(|i| self.incompressible.insert(self.file(i)));
        self.stats
            .incompressible_files
            .fetch_add(1, Ordering::Relaxed);
        self.stats
            .incompressible_bytes
            .fetch_add(alloc, Ordering::Relaxed);
    }

    /// Compresses file `i`, which alone uses `owned` bytes.
    fn compress_single(&self, i: usize, owned: u64) {
        let f = self.file(i);
        let path = self.scan.path(f);
        let dir = match self.dir(f) {
            Ok(d) => d,
            Err(e) => return self.skip("compress", &path, &e),
        };
        match compress::prepare(&dir, f, &self.opts.compression) {
            Ok(Outcome::Compressed {
                prepared,
                new_alloc,
            }) => {
                let Some(prepared) = prepared else {
                    return self.count_compressed(owned, new_alloc);
                };
                match prepared.commit(f, true) {
                    Ok((replaced, old)) => {
                        self.count_compressed(owned, new_alloc);
                        self.report(f, replaced);
                        self.check_released(f, old, owned);
                    }
                    Err(e) => self.skip("compress", &path, &e),
                }
            }
            Ok(Outcome::NotWorthIt) => self.not_worth_it(std::iter::once(i), f.alloc),
            Err(e) => self.skip("compress", &path, &e),
        }
    }

    /// Opens file `i` if it is unchanged since the scan and has no metadata that a replacement or
    /// a clone of it cannot keep.
    fn open_replaceable(&self, i: usize) -> Option<OwnedFd> {
        let f = self.file(i);
        let fd = self.dir(f).and_then(|d| d.open_file(&f.name)).ok()?;
        replace::unchanged_fd(&fd, f.into()).ok()?;
        Meta::read(&fd, f).ok()?;
        Some(fd)
    }

    /// Replaces `files` with clones of `source` (`None` in a dry run). Returns the replaced files
    /// with the identities of their clones, and if `keep_one`, the first replaced file, still open.
    fn clone_files(
        &self,
        source: Option<(&OwnedFd, Identity)>,
        files: &[usize],
        keep_one: bool,
    ) -> (Vec<(usize, Option<Identity>)>, Option<OwnedFd>) {
        let mut done = Vec::new();
        let mut first_old = None;
        for &t in files {
            let f = self.file(t);
            let res = match source {
                None => Ok(None),
                Some((fd, id)) => self
                    .dir(f)
                    .and_then(|dir| {
                        let keep = keep_one && first_old.is_none();
                        dedupe::replace_with_clone(fd, id, &dir, f, keep)
                    })
                    .map(Some),
            };
            match res {
                Ok(None) => done.push((t, None)),
                Ok(Some(c)) => {
                    self.report(f, c.replaced);
                    first_old = first_old.or(c.old);
                    done.push((t, c.clone));
                }
                Err(e) => self.skip("clone", &self.scan.path(f), &e),
            }
        }
        (done, first_old)
    }

    fn count_cloned(&self, files: impl Iterator<Item = usize>) {
        for i in files {
            self.stats.cloned_files.fetch_add(1, Ordering::Relaxed);
            self.stats
                .cloned_bytes
                .fetch_add(self.file(i).size, Ordering::Relaxed);
        }
    }

    /// Replaces the files `rest` of a family with clones of `source`, but not its file `last`,
    /// which is then the last file under the scanned paths that uses the family's blocks. If
    /// files outside the scanned paths share more than a tenth of them, replacing `last` as well
    /// would not free them, and the new copy would take space in addition. In that case the
    /// replaced files become clones of `last` again.
    fn replace_all_but(
        &self,
        source: Option<(&OwnedFd, Identity)>,
        rest: &[usize],
        last: usize,
        owned: u64,
    ) -> AllBut {
        let (done, _) = self.clone_files(source, rest, false);
        if done.len() < rest.len() {
            return AllBut::Partial(done.into_iter().map(|(t, _)| t).collect());
        }
        let probe = source.and_then(|_| self.open_replaceable(last));
        let Some(last_fd) = probe.filter(|fd| self.shared_elsewhere(fd, owned) * 10 > owned) else {
            return AllBut::Done(done.into_iter().map(|(t, _)| t).collect());
        };
        AllBut::SharedOutside(self.restore(done, &last_fd, last))
    }

    /// Makes the `replaced` files, given with the identities of their clones, clones of the open
    /// file `fd`, the unchanged file `i`, again. Returns the files that could not be.
    fn restore(
        &self,
        replaced: Vec<(usize, Option<Identity>)>,
        fd: &OwnedFd,
        i: usize,
    ) -> Vec<usize> {
        let id = Identity::from(self.file(i));
        let mut left = Vec::new();
        for (t, clone) in replaced {
            let f = self.file(t);
            let restored = clone.and_then(|c| {
                let now = File {
                    ino: c.ino,
                    ctime: c.ctime,
                    mtime: c.mtime,
                    size: c.size,
                    generation: c.generation,
                    ..f.clone()
                };
                let dir = self.dir(f).ok()?;
                dedupe::replace_with_clone(fd, id, &dir, &now, false).ok()
            });
            match restored {
                Some(c) => self.report(f, c.replaced),
                None => left.push(t),
            }
        }
        left
    }

    /// Counts the files of `fam` as left alone because files outside the scanned paths share
    /// their blocks.
    fn skip_shared_outside(&self, fam: &Family) {
        let e = replace::unsupported(SHARED_OUTSIDE);
        for &i in fam {
            self.skip("compress", &self.scan.path(self.file(i)), &e);
        }
    }

    /// Makes all families of identical files share one copy, compressing it first when the
    /// files under the scanned paths are the only users of their blocks.
    fn run_group(&self, families: &[Family]) {
        let all = || families.iter().flatten().copied();
        let compressed = all().find(|&i| self.file(i).is_compressed());

        if compressed.is_none()
            && self.opts.compress
            && !families.iter().any(|fam| self.known_incompressible(fam))
        {
            // A family that is only partly replaced keeps its blocks allocated in addition to the
            // compressed copy.
            let replaceable: Vec<Family> = families
                .iter()
                .filter(|fam| fam.iter().all(|&i| self.open_replaceable(i).is_some()))
                .cloned()
                .collect();
            let owned: Vec<Option<u64>> = replaceable
                .iter()
                .map(|fam| self.owned_bytes(fam))
                .collect();
            // Full clones outside the scanned paths keep a family's blocks allocated in any case,
            // so the other files should become clones of them rather than of a compressed copy.
            let kept_outside = replaceable
                .iter()
                .zip(&owned)
                .any(|(fam, o)| o.is_none() && self.file(fam[0]).is_pure_clone());
            if !kept_outside {
                for (k, fam) in replaceable.iter().enumerate() {
                    if owned[k].is_none() {
                        continue;
                    }
                    match self.compress_into_group(&replaceable, k, &owned) {
                        Ok(true) => return,
                        Ok(false) => {
                            self.not_worth_it(all(), self.file(fam[0]).alloc);
                            break;
                        }
                        // Another copy may lack what kept this one from being replaced, e.g. an ACL.
                        Err(e) => self.skip("compress", &self.scan.path(self.file(fam[0])), &e),
                    }
                }
            }
        }
        if families.len() < 2 || !self.opts.dedupe {
            return;
        }
        // Keep a compressed copy, or else the one that already shares its blocks with the most files.
        let member = |k: usize| {
            compressed
                .filter(|c| families[k].contains(c))
                .unwrap_or(families[k][0])
        };
        let mut order: Vec<usize> = (0..families.len()).collect();
        order.sort_by_cached_key(|&k| {
            let f = families[k][0];
            let refcnt = if self.file(f).is_pure_clone() {
                self.clone_refcnt(f)
            } else {
                1
            };
            std::cmp::Reverse((
                self.file(member(k)).is_compressed(),
                refcnt,
                families[k].len(),
            ))
        });
        let Some((source_fam, source_fd)) = order
            .into_iter()
            .find_map(|k| self.open_replaceable(member(k)).map(|fd| (k, fd)))
        else {
            return;
        };
        let source_id = Identity::from(self.file(member(source_fam)));
        let source = (!self.opts.dry_run).then_some((&source_fd, source_id));
        for (k, fam) in families.iter().enumerate() {
            if k == source_fam {
                continue;
            }
            let owned = self.owned_bytes(fam);
            let (done, old) = self.clone_files(source, fam, true);
            self.count_cloned(done.iter().map(|&(t, _)| t));
            if done.len() == fam.len() {
                if let Some(b) = owned {
                    self.stats.dedupe_freed.fetch_add(b, Ordering::Relaxed);
                }
                self.check_released(self.file(fam[0]), old, owned.unwrap_or(0));
            }
        }
    }

    /// Compresses the first file of `families[k]` and makes all files of `families` clones of
    /// the compressed copy. `owned` are the bytes each family owns, `None` for a family that
    /// shares some of its blocks with other files, `families[k]` included. Returns false if
    /// compressing is not worth it.
    fn compress_into_group(
        &self,
        families: &[Family],
        k: usize,
        owned: &[Option<u64>],
    ) -> io::Result<bool> {
        let first = families[k][0];
        let f = self.file(first);
        let dir = self.dir(f)?;
        let Outcome::Compressed {
            prepared,
            new_alloc,
        } = compress::prepare(&dir, f, &self.opts.compression)?
        else {
            return Ok(false);
        };
        // The compressed copy is not in place yet, so nothing else can change it while it is
        // cloned. The file it replaces goes last, and only if the rest of its family was replaced.
        let source = prepared.as_ref().map(|p| (&p.fd, p.id));
        let dry_run = prepared.is_none();
        let mut cloned: Vec<usize> = Vec::new();
        let mut partial = false;
        let mut released: Vec<usize> = Vec::new();
        // Replacing a family that shares its blocks frees little by itself, so it is replaced
        // first. Afterwards, the private size of the families it shares them with shows whether
        // files outside the scanned paths share them too.
        let mut shared: Vec<(usize, Option<Identity>)> = Vec::new();
        for j in (0..families.len()).filter(|&j| owned[j].is_none()) {
            shared.extend(self.clone_files(source, &families[j], false).0);
        }
        for j in (0..families.len()).filter(|&j| j != k && owned[j].is_some()) {
            let owned = owned[j].unwrap();
            let (&last, rest) = families[j].split_last().unwrap();
            match self.replace_all_but(source, rest, last, owned) {
                AllBut::Done(done) => {
                    let (last_done, _) = self.clone_files(source, &[last], false);
                    if last_done.is_empty() {
                        partial |= !done.is_empty();
                    } else {
                        cloned.push(last);
                        released.push(j);
                    }
                    cloned.extend(done);
                }
                AllBut::Partial(done) => {
                    partial |= !done.is_empty();
                    cloned.extend(done);
                }
                AllBut::SharedOutside(left) => {
                    self.skip_shared_outside(&families[j]);
                    partial |= !left.is_empty();
                    cloned.extend(left);
                }
            }
        }
        let owned_k = owned[k].expect("the compressed family owns its blocks");
        let first_replaced = match self.replace_all_but(source, &families[k][1..], first, owned_k) {
            AllBut::Done(done) => {
                cloned.extend(done);
                match prepared {
                    None => {
                        released.push(k);
                        true
                    }
                    Some(p) => match p.commit(f, false) {
                        Ok((replaced, _)) => {
                            self.report(f, replaced);
                            released.push(k);
                            true
                        }
                        Err(e) => {
                            self.skip("compress", &self.scan.path(f), &e);
                            partial |= families[k].len() > 1;
                            false
                        }
                    },
                }
            }
            AllBut::Partial(done) => {
                partial = true;
                cloned.extend(done);
                false
            }
            AllBut::SharedOutside(left) => {
                self.skip_shared_outside(&families[k]);
                partial |= !left.is_empty();
                cloned.extend(left);
                false
            }
        };
        // If no family's blocks were freed, the compressed copy only takes space in addition to
        // the blocks that the families in `shared` keep allocated.
        if released.is_empty() && !dry_run && !shared.is_empty() {
            let left = match self.open_replaceable(first) {
                Some(fd) => self.restore(shared, &fd, first),
                None => shared.into_iter().map(|(t, _)| t).collect(),
            };
            partial |= !left.is_empty();
            cloned.extend(left);
        } else {
            cloned.extend(shared.into_iter().map(|(t, _)| t));
        }
        let used = !cloned.is_empty();
        self.count_cloned(cloned.into_iter());
        if partial && (used || first_replaced) {
            self.stats.partial_groups.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "warning: some files identical to {} could not be replaced, so their blocks stay allocated besides the compressed copy",
                self.scan.path(f).display()
            );
        }
        if used || first_replaced {
            self.stats.compressed_files.fetch_add(1, Ordering::Relaxed);
            self.stats
                .compressed_after
                .fetch_add(new_alloc, Ordering::Relaxed);
        }
        // `replace_all_but` checked that each released family's blocks are freed.
        for j in released {
            let owned = owned[j].unwrap();
            if j == k {
                self.stats
                    .compressed_before
                    .fetch_add(owned, Ordering::Relaxed);
            } else {
                self.stats.dedupe_freed.fetch_add(owned, Ordering::Relaxed);
            }
        }
        Ok(true)
    }
}

/// How replacing all files of a family but its last one went.
enum AllBut {
    /// The last one is the only file under the scanned paths that uses the family's blocks.
    Done(Vec<usize>),
    /// Only these files could be replaced.
    Partial(Vec<usize>),
    /// Files outside the scanned paths share the blocks, so the replaced files were made clones
    /// of the last one again, except these.
    SharedOutside(Vec<usize>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::tests::{tempdir, text};
    use crate::compress::{Algorithm, Settings};
    use std::collections::HashSet;
    use std::os::unix::fs::FileExt;
    use std::path::PathBuf;

    const MIB: u64 = 1 << 20;
    const SHARED: u64 = 64 << 10;

    /// A directory `root` with the file `a`, which shares its last 64 KiB with a file outside
    /// `root`, and if `duplicate`, the file `b` with the same content as `a` but its own blocks.
    fn partly_shared(duplicate: bool) -> PathBuf {
        let base = std::fs::canonicalize(tempdir()).unwrap();
        let root = base.join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(base.join("outside"), text(MIB as usize, 1)).unwrap();
        let src = sys::open(&base.join("outside"), libc::O_RDONLY).unwrap();
        let dir = sys::open(&root, libc::O_RDONLY | libc::O_DIRECTORY).unwrap();
        sys::fclonefileat(src.as_raw_fd(), dir.as_raw_fd(), c"a").unwrap();
        let a = std::fs::OpenOptions::new()
            .write(true)
            .open(root.join("a"))
            .unwrap();
        a.write_all_at(&text((MIB - SHARED) as usize, 2), 0)
            .unwrap();
        a.sync_all().unwrap();
        let attrs = sys::file_attrs(&root.join("a")).unwrap();
        assert_eq!((attrs.alloc, attrs.private_size), (MIB, MIB - SHARED));
        if duplicate {
            std::fs::write(root.join("b"), std::fs::read(root.join("a")).unwrap()).unwrap();
        }
        root
    }

    fn run(root: PathBuf) -> (Plan, Stats) {
        let scan = crate::scan::scan(
            &[root],
            &crate::scan::Options {
                exclude: &[],
                open_for_writing: &HashSet::new(),
                min_size: 1,
                dry_run: false,
            },
            &crate::scan::ScanStats::default(),
        );
        let opts = Options {
            dry_run: false,
            dedupe: true,
            compress: true,
            min_size: 1,
            verbose: true,
            compression: Settings {
                algorithm: Algorithm::Auto,
                dry_run: false,
                min_saving: 0.1,
                durable: false,
            },
        };
        let runner = Runner::new(&scan, &opts, Incompressible::load(None, 0));
        let plan = runner.plan();
        runner.execute(&plan);
        (plan, runner.finish())
    }

    fn get(a: &AtomicU64) -> u64 {
        a.load(Ordering::Relaxed)
    }

    #[test]
    fn a_compressed_file_frees_only_the_blocks_it_does_not_share() {
        let (plan, s) = run(partly_shared(false));

        assert_eq!((plan.groups.len(), plan.singles.len()), (0, 1));
        assert_eq!(get(&s.compressed_before), MIB - SHARED);
        assert_eq!(get(&s.still_shared), 0);
        assert_eq!(s.freed(), MIB - SHARED - get(&s.compressed_after));
    }

    #[test]
    fn identical_files_free_only_the_blocks_they_do_not_share() {
        let (plan, s) = run(partly_shared(true));

        assert_eq!((plan.groups.len(), plan.singles.len()), (1, 0));
        assert_eq!(
            get(&s.compressed_before) + get(&s.dedupe_freed),
            MIB + MIB - SHARED
        );
        assert_eq!(get(&s.still_shared), 0);
        assert_eq!(s.freed(), MIB + MIB - SHARED - get(&s.compressed_after));
    }

    fn compressed(path: &std::path::Path) -> bool {
        std::os::macos::fs::MetadataExt::st_flags(&std::fs::metadata(path).unwrap())
            & libc::UF_COMPRESSED
            != 0
    }

    /// Like an environment that a package manager cloned from its cache and then rewrote in part
    /// (e.g. to sign it), and then cloned once more: `env/a` and `env/b` are full clones of each
    /// other, which share all but their first 64 KiB with `cache/file`. Returns the directory of
    /// both and the content of all three files.
    fn cloned_from_cache() -> (PathBuf, Vec<u8>) {
        let base = std::fs::canonicalize(tempdir()).unwrap();
        let (env, cache) = (base.join("env"), base.join("cache"));
        std::fs::create_dir(&env).unwrap();
        std::fs::create_dir(&cache).unwrap();
        let data = text(MIB as usize, 3);
        std::fs::write(cache.join("file"), &data).unwrap();
        let cached = sys::open(&cache.join("file"), libc::O_RDONLY).unwrap();
        let dir = sys::open(&env, libc::O_RDONLY | libc::O_DIRECTORY).unwrap();
        sys::fclonefileat(cached.as_raw_fd(), dir.as_raw_fd(), c"a").unwrap();
        let a = std::fs::OpenOptions::new()
            .write(true)
            .open(env.join("a"))
            .unwrap();
        a.write_all_at(&data[..SHARED as usize], 0).unwrap();
        a.sync_all().unwrap();
        drop(a);
        let a = sys::open(&env.join("a"), libc::O_RDONLY).unwrap();
        sys::fclonefileat(a.as_raw_fd(), dir.as_raw_fd(), c"b").unwrap();
        let attrs = sys::file_attrs(&env.join("a")).unwrap();
        assert_eq!((attrs.clone_refcnt, attrs.private_size), (2, 0));
        (base, data)
    }

    #[test]
    fn compresses_files_together_with_the_cache_they_were_cloned_from() {
        let (base, data) = cloned_from_cache();

        let (plan, s) = run(base.clone());

        assert_eq!((plan.groups.len(), plan.singles.len()), (1, 0));
        assert_eq!(get(&s.compressed_files), 1);
        assert_eq!(get(&s.still_shared), 0);
        assert!(s.skipped.lock().unwrap().is_empty());
        for path in ["env/a", "env/b", "cache/file"] {
            assert!(compressed(&base.join(path)), "{path}");
            assert_eq!(std::fs::read(base.join(path)).unwrap(), data, "{path}");
        }
        assert_eq!(get(&s.cloned_files), 2);
        assert_eq!(get(&s.compressed_before), MIB);
        for path in ["env/a", "env/b", "cache/file"] {
            let ext = sys::ext_flags(&base.join(path)).unwrap();
            assert_ne!(ext & sys::EF_SHARES_ALL_BLOCKS, 0, "{path} is a clone");
        }
    }

    #[test]
    fn keeps_no_compressed_copy_if_files_outside_share_all_blocks() {
        let (base, data) = cloned_from_cache();
        let root = base.join("env");
        // A second partial clone of the cache, which shares its blocks with no file under `root`.
        let cached = sys::open(&base.join("cache/file"), libc::O_RDONLY).unwrap();
        let dir = sys::open(&root, libc::O_RDONLY | libc::O_DIRECTORY).unwrap();
        sys::fclonefileat(cached.as_raw_fd(), dir.as_raw_fd(), c"c").unwrap();
        let c = std::fs::OpenOptions::new()
            .write(true)
            .open(root.join("c"))
            .unwrap();
        c.write_all_at(&data[..SHARED as usize], 0).unwrap();
        c.sync_all().unwrap();

        let (plan, s) = run(root.clone());

        assert_eq!((plan.groups.len(), plan.singles.len()), (1, 0));
        assert_eq!(get(&s.compressed_files), 0);
        assert_eq!(get(&s.partial_groups), 0);
        for name in ["a", "b", "c"] {
            assert!(!compressed(&root.join(name)), "{name}");
            assert_eq!(std::fs::read(root.join(name)).unwrap(), data, "{name}");
        }
        let attrs = sys::file_attrs(&root.join("c")).unwrap();
        assert_eq!(
            (attrs.clone_refcnt, attrs.private_size),
            (3, 0),
            "c became a clone of a and b"
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 3);
    }

    #[test]
    fn leaves_clones_alone_whose_blocks_files_outside_share() {
        let (base, data) = cloned_from_cache();
        let root = base.join("env");

        let (plan, s) = run(root.clone());

        assert_eq!((plan.groups.len(), plan.singles.len()), (1, 0));
        assert_eq!(get(&s.compressed_files), 0);
        assert_eq!(get(&s.still_shared_files), 0);
        assert_eq!(
            s.skipped.lock().unwrap().get(SHARED_OUTSIDE).copied(),
            Some(2)
        );
        for name in ["a", "b"] {
            assert!(!compressed(&root.join(name)), "{name}");
            assert_eq!(std::fs::read(root.join(name)).unwrap(), data, "{name}");
        }
        let attrs = sys::file_attrs(&root.join("a")).unwrap();
        assert_eq!(
            (attrs.clone_refcnt, attrs.private_size),
            (2, 0),
            "a and b are clones of each other again"
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
    }
}
