# reclaim

`reclaim` frees disk space on APFS volumes (macOS 27 or later, Apple silicon). It replaces identical
files with APFS clones of one copy and compresses files whose blocks no other file shares with APFS
transparent compression. Its defaults favor run time over saved space. It is meant for directories
that no process writes to while it runs, see [Safety](#safety).

## Usage

Install `reclaim` from the repo root with a Rust toolchain and run it on the directories to
process:

```sh
cargo install --path .
reclaim ~/code                  # all repos and worktrees
reclaim -n ~/Downloads          # dry run: report only
reclaim ~/code ~/Library/Caches/rattler ~/Library/pnpm/store
```

Package managers such as pixi (with its package cache `~/Library/Caches/rattler`) and pnpm clone
files from their caches into the projects. `reclaim` only compresses these files if their caches are among the given paths,
as described in [Clones](#clones). A repeated run takes a fraction of the time of the first one,
because it does not hash clones and skips files that are already compressed or that an earlier run
found not worth compressing. `reclaim --help` lists all options.

## How it works

A run has four steps:

1. `reclaim` lists all given directories in parallel with `getattrlistbulk`. This system call
   returns the attributes of all entries of a directory, including the APFS clone attributes, in a
   few calls.
2. It compares files of the same size by their BLAKE3 hash, first of their first and last 4 KiB
   and, only if those match, of the whole file. Files of the same size with different content, such
   as `.pyc` files of other package versions, mostly differ in these parts already, for example in
   a header or in a code signature at the end. Files with the same clone ID that share all their
   blocks (pure clones) count as one copy without hashing.
3. It replaces each group of identical files with clones of one copy. It prefers a compressed copy
   as the source, and otherwise the copy that already shares its blocks with the most files. This
   way the files of a new worktree become clones of files that are already compressed.
4. It compresses files that share at most a tenth of their blocks with other files. Clones are only
   compressed under the conditions in [Clones](#clones). Files that do not get at least 10% smaller
   and save at least one 4 KiB block stay as they are. `reclaim` records them in
   `~/Library/Caches/reclaim`, so later runs do not read them again until they change or the
   options `--algorithm` or `--min-saving` change.

### Clones

Compressing a clone writes the compressed data to new blocks, while the old blocks stay allocated
for the other clones. `reclaim` therefore compresses clones only if all files that share their
blocks are under the given paths and all of them can be replaced. It compares the APFS clone
reference count with the number of clones under the given paths to check the first condition. One
compressed copy then replaces all of these files. If some of them cannot be replaced after all, the
others are already clones of the compressed copy, and the remaining files keep their old blocks.
`reclaim` warns about such groups. Without the package caches among the given paths, every file in
a `.pixi/envs` directory is a clone of a file in the rattler cache and stays uncompressed.

The clone reference count only counts full clones. pixi, for example, changes parts of some files
after cloning them from its cache. Such a partial clone still shares most blocks with the cache
file, but the reference count does not include it. If a partial clone lies outside the given
paths, compressing the files under the paths costs space instead of saving it. The APFS private
size of a file only shows this once no other file under the given paths uses its blocks. Therefore
`reclaim` replaces all clones under the given paths but one and then reads the private size of the
remaining one. If more than a tenth of that file's blocks are still shared, `reclaim` makes the
replaced files clones of it again, leaves them uncompressed and prints a hint to add the cache
directory to the given paths.

If the cache is among the given paths, the cache file and the partial clones have identical
content, so `reclaim` compresses them together in the same run.

When `reclaim` only clones files without compressing them, it also warns when the blocks of a
replaced file stay allocated, and the reported savings leave out those blocks.

### Compression

`reclaim` writes compressed files in decmpfs, the format of APFS transparent compression. By
default (`-a auto`), it uses LZFSE for files up to 32 KiB and LZBITMAP for larger ones, because
LZBITMAP is faster but compresses small files badly. For a small file, a saved 4 KiB block is a
large share of its size. The following measurement on 1.5 GB of worktree files with 10 threads
compares the algorithms:

| algorithm | compress | space saved | decompress (CPU) |
|-----------|---------:|------------:|-----------------:|
| LZBITMAP  | 1270 MB/s | 56% | 0.4 s |
| LZFSE     |  570 MB/s | 60% | 1.2 s |
| LZVN      |  430 MB/s | 57% | 1.2 s |
| zlib      |  280 MB/s | 60% | 3.0 s |

`reclaim` reads every compressed file back through the kernel and compares it with the original
before it replaces the original. Reading LZBITMAP files is only verified on macOS 27. If macOS
versions before 27 may read the compressed files, use `-a lzfse`.

`reclaim` skips files below 16 KiB by default (`--min-size`). Replacing a file costs about 0.2 to
0.3 ms of kernel time regardless of its size, so small files take most of the time but save little.
In the [benchmark](#benchmark), files of up to 16 KiB were 84% of all replacements but only 12% of
the saved space.

## Safety

`reclaim` cannot stop other processes from writing to the files it processes. These measures keep
the time window short in which such a write can get lost:

- `reclaim` replaces a file only if its inode, size, ctime, mtime and APFS generation count still
  have the values from the scan. Every write changes the ctime and the generation count, and
  processes cannot set either. While a process has a file mapped for writing, the file's generation
  count is 0, and `reclaim` skips the file.
- At the start, `reclaim` lists the files that processes of the same user have open for writing
  (with `proc_pidinfo`) and skips them. Without root, it cannot list the open files of processes of
  other users, including root, and it does not detect files that a process opens later.
- `reclaim` exchanges the directory entries of the new and the old file in one atomic operation
  (`renameatx_np` with `RENAME_SWAP`). After the swap, it checks both files once more. The swap
  changes the old file's ctime, so the check compares the rest of what a write or a change of
  permissions changes: generation count, size, mtime, mode, owner, flags, ACL and extended
  attributes. The app grants (`com.apple.macl`) are the exception, because the kernel keeps this
  extended attribute with the path and exchanges it with the swap.

If the old file changed or was replaced between the last check and the swap, for example by an
editor's atomic save, `reclaim` keeps it as `<name>.reclaim-changed` next to the new file and
prints a warning. A write that happens after the swap through a descriptor or mapping that was
opened before it is lost.

### Renamed files and directories

`reclaim` compares, compresses and clones every file through the descriptor it checked the file
with. It runs all operations through a descriptor of the file's directory, after checking that this
directory is the one from the scan. Renaming files or directories during the run therefore cannot
make `reclaim` clone, replace or remove other content.

### Skipped files

`reclaim` leaves these files alone:

- hard links and files of other users
- files with an ACL or with an ACL that cannot be read, and files with a resource fork
- iCloud placeholders (dataless files) and purgeable files, which macOS may delete when space runs
  low
- immutable, append-only and otherwise protected files (see `BLOCKING_FLAGS` in
  [src/scan.rs](src/scan.rs))
- everything inside iCloud or File Provider sync roots, also when a given path lies inside one

### Metadata

A replaced file keeps its content, mode, owner and group, flags, extended attributes, and its
creation, modification, access and "date added" times. Directories keep their modification times
unless something else changed them during the run. The inode number of a replaced file changes, so
`git status` reads its content once more and Time Machine backs it up once more.

### Crashes and interruptions

Before the swap, `reclaim` writes the compressed data to the drive with `fsync`, which protects it
against crashes and kernel panics. The drive may still hold the data in its volatile cache. Only
`--durable` (`F_BARRIERFSYNC`, about 1 ms per compressed file) makes sure that the drive stores the
data permanently before the swap. After a power loss, the file is then either the original or the
complete compressed file. Neither mode makes the swap itself persistent, so a power loss may undo
the last replacements. Power loss is untested.

Ctrl-C stops the run after the files in progress, and a second Ctrl-C kills the process. A killed
run leaves temporary files named `.reclaim-tmp.<pid>.<n>`. The next run removes such a file if it
has the extended attribute that `reclaim` sets on its temporary files, if `--exclude` does not
exclude its name and if the process that created it no longer exists.

APFS snapshots, such as the local snapshots of Time Machine, keep the blocks of replaced files
allocated until the snapshots are deleted.

## Benchmark

The benchmark used three worktrees of a large Python and TypeScript monorepo, copied with `cp -R`,
so that no file was a clone and none was compressed. They had 496,404 files with 17.4 GiB logical and 18.5 GiB allocated size. A
comparison of all files before and after the run (content, mode, flags, times and extended
attributes) found no differences.

| | first run | freed | second run |
|-|----------:|------:|-----------:|
| Python script (dedupe by hash, then applesauce) | 62 s | 8.8 GiB | ≈7 s |
| `reclaim` | **40 s** | **12.3 GiB** | **2 s** |
| `reclaim --min-size 1` (also small files) | ≈150 s | 13.7 GiB | |

On a copy of `~/Downloads` with 25k files and 17.5 GiB, mostly media and archives, `reclaim` took
8.5 s and freed 2.0 GiB, half of it from duplicate downloads.

## Code map

`reclaim` is a single binary. [src/main.rs](src/main.rs) parses the options, runs the scan, plans
the replacements and executes them. The modules have these tasks:

- [src/scan.rs](src/scan.rs) walks the given directories and collects the files that may be
  replaced.
- [src/run.rs](src/run.rs) decides what happens to each file and carries it out, including the
  handling of clones described in [Clones](#clones).
- [src/dedupe.rs](src/dedupe.rs) groups files with identical content and replaces a file with a
  clone.
- [src/compress.rs](src/compress.rs) writes a compressed copy of a file in the decmpfs format.
- [src/replace.rs](src/replace.rs) swaps a new file into the place of an old one and copies the
  old file's metadata to it.
- [src/cache.rs](src/cache.rs) stores the files that did not compress well enough.
- [src/sys.rs](src/sys.rs) wraps the macOS system calls and libcompression.
- [tests/cli.rs](tests/cli.rs) runs the binary on a directory tree and checks afterwards that every
  file reads and looks the same.

`DirFd` in [src/replace.rs](src/replace.rs) implements the directory check from
[Renamed files and directories](#renamed-files-and-directories), so every new operation on a scanned
file must go through it. `cargo test` creates its files in the system's temporary directory, so it
needs macOS with an APFS system volume.
