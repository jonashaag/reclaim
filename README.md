# reclaim

`reclaim` frees disk space on macOS (APFS volumes).
It replaces identical files with APFS clones of one copy and compresses files with APFS transparent compression.
Your files keep their content, permissions and timestamps, and programs read them as before.

On my disk, the first run freed around 50 GiB and took a couple of minutes.
Later runs are much faster, because they skip files that are already clones or compressed.

Do not change files in the directories that `reclaim` is working on.
It is designed not to damage your files, but it may leave copies of changed files that you need to clean up yourself, see [Safety](#safety).

`reclaim` needs macOS 27 or later on Apple silicon.

NOTE: Reclaim has been written by Opus 5.5 and reviewed for safety by Astra max.

## Install

```sh
curl -fsSL https://github.com/jonashaag/reclaim/releases/latest/download/reclaim-installer.sh | sh
```

## Usage

Pass the directories that `reclaim` should process.
A dry run shows how much space a run would free, without changing anything:

```sh
reclaim --dry-run ~/Downloads
reclaim ~/Downloads
```

`reclaim --help` lists all options.

### Package caches

Package managers such as pixi and pnpm clone files from their caches into your projects.
`reclaim` only compresses these files if you also pass the caches:

```sh
reclaim ~/code ~/Library/Caches/rattler ~/Library/pnpm/store
```

Compressing only the clones in your projects would store their content twice, once compressed in the project and once uncompressed in the cache.
`reclaim` therefore leaves such files uncompressed.
If it prints a hint to pass the directories these files were cloned from, add the cache to the paths.

## What to expect

- `reclaim` skips files below 16 KiB by default (`--min-size`), because they take most of the time but free little space.
- It leaves hard links, files of other users, files with ACLs, iCloud files and files that your programs have open for writing alone.
- A replaced file gets a new inode number. `git status` therefore reads it once more, and Time Machine backs it up once more.
- Time Machine local snapshots keep the old blocks until they are deleted, so the free space may only grow after that.
  Note: Interaction with and potential side effects on Time Machine are untested.
- Files compressed with the default algorithm are only verified to be readable on macOS 27. If older macOS versions may read them, use `--algorithm lzfse`.

## Safety

`reclaim` checks every file right before and after replacing it.
If a file changed in between, `reclaim` keeps the changed version as `<name>.reclaim-changed` next to the file and prints a warning.
You then need to check these files and clean them up yourself.
A program that opened a file before `reclaim` replaced it and writes to it afterwards loses that write.

You can stop `reclaim` with Ctrl-C at any time.
It finishes the files in progress and stops, and a second Ctrl-C kills it immediately.
The next run removes the temporary files `.reclaim-tmp.*` that a killed run left behind.

If you are worried about power loss during a run, use `--durable`.
`reclaim` then makes sure that each compressed file is completely on disk before it replaces the original, at about 1 ms per file.

## How it works

`reclaim` lists the given directories with `getattrlistbulk`, which returns the APFS clone attributes of a whole directory in a few system calls.
It finds identical files by comparing BLAKE3 hashes of files with the same size, first of their first and last 4 KiB and then of the whole file.
Each group of identical files becomes clones of one copy, preferably of a copy that is already compressed.

It compresses the other files in the decmpfs format of APFS, with LZFSE for files up to 32 KiB and the faster LZBITMAP for larger ones.
Files that do not get at least 10% smaller stay as they are, and `reclaim` remembers them in `~/Library/Caches/reclaim` so that later runs skip them.
Before a compressed copy replaces a file, `reclaim` reads it back through the kernel and compares it with the original.

Clones are only compressed if all files sharing their blocks are under the given paths, because otherwise the shared blocks stay allocated next to the compressed copy.
The APFS clone reference count shows this for full clones.
Clones that were changed in part after cloning are not counted, so `reclaim` checks the private size of the last of them before replacing it, and undoes the replacements if files outside the given paths still share the blocks.

The new file takes the place of the old one with an atomic swap (`renameatx_np` with `RENAME_SWAP`).
All operations go through descriptors of files and directories that `reclaim` checked against the scan, so renaming files during a run cannot make it act on the wrong file.

`cargo test` needs macOS with an APFS system volume, because the tests create clones and compressed files in the temporary directory.
