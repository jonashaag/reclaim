//! Runs the binary on a directory tree with duplicates, compressible and incompressible files and
//! files it must not touch, and checks that every file still reads and looks the same.

use std::ffi::CString;
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn text(len: usize, seed: u64) -> Vec<u8> {
    let words: [&[u8]; 6] = [
        b"alpha ",
        b"beta ",
        b"gamma ",
        b"return ",
        b"import os\n",
        b"x = 1\n",
    ];
    let mut x = seed | 1;
    let mut out = Vec::new();
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(words[(x % 6) as usize]);
    }
    out.truncate(len);
    out
}

fn random(len: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn xattrs(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut names = vec![0u8; 4096];
    let n = unsafe {
        libc::listxattr(
            c.as_ptr(),
            names.as_mut_ptr().cast(),
            names.len(),
            libc::XATTR_NOFOLLOW,
        )
    };
    let mut out = Vec::new();
    for name in names[..n.max(0) as usize]
        .split(|&b| b == 0)
        .filter(|n| !n.is_empty() && *n != b"com.apple.provenance")
    {
        let cn = CString::new(name).unwrap();
        let mut v = vec![0u8; 65536];
        let m = unsafe {
            libc::getxattr(
                c.as_ptr(),
                cn.as_ptr(),
                v.as_mut_ptr().cast(),
                v.len(),
                0,
                libc::XATTR_NOFOLLOW,
            )
        };
        v.truncate(m.max(0) as usize);
        out.push((name.to_vec(), v));
    }
    out.sort();
    out
}

/// Content and the metadata that must survive, per file; plus the inode when `with_inode`.
fn snapshot(root: &Path, with_inode: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                stack.push(p);
                continue;
            }
            let content = blake3::hash(&std::fs::read(&p).unwrap());
            let ino = if with_inode { m.ino() } else { 0 };
            out.push(format!(
                "{} {} {:o} {:#x} {}.{} {:?} {} {:?} {ino}",
                p.strip_prefix(root).unwrap().display(),
                content.to_hex(),
                m.mode(),
                m.st_flags() & !libc::UF_COMPRESSED,
                m.mtime(),
                m.mtime_nsec(),
                m.created().unwrap(),
                m.size(),
                xattrs(&p),
            ));
        }
    }
    out.sort();
    out
}

fn compressed(path: &Path) -> bool {
    std::fs::symlink_metadata(path).unwrap().st_flags() & libc::UF_COMPRESSED != 0
}

fn run(args: &[&str], root: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_reclaim"))
        .args(args)
        .arg("--no-cache")
        .arg(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn fixture() -> PathBuf {
    let root = std::env::temp_dir().join(format!("reclaim-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["a", "b", "c/deep"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let write = |p: &str, data: &[u8]| std::fs::write(root.join(p), data).unwrap();
    let source = text(300_000, 1);
    write("a/source.py", &source);
    write("b/source.py", &source);
    write("c/deep/source.py", &source);
    write("a/big.txt", &text(5_000_000, 2));
    write("a/random.bin", &random(400_000));
    write("b/random.bin", &random(400_000));
    write("a/small.txt", &text(20_000, 3));
    write("a/tiny.txt", &text(1000, 4));
    write("a/linked.txt", &text(100_000, 5));
    std::fs::hard_link(root.join("a/linked.txt"), root.join("b/linked.txt")).unwrap();
    write("a/open.txt", &text(100_000, 6));
    let sh = |cmd: &str| sh(&root, cmd);
    sh(
        "xattr -w user.note hi b/source.py && chmod 444 b/source.py && chflags hidden c/deep/source.py",
    );
    sh("touch -t 202001020304 b/random.bin");
    sh("cp -c a/big.txt b/big-clone.txt");
    // Left behind by a run that was killed; pid 99999 does not exist on macOS (pids end below).
    write("c/.reclaim-tmp.99999.0", b"partial");
    sh("xattr -w io.github.jonashaag.reclaim.tmp x c/.reclaim-tmp.99999.0");
    root
}

fn sh(cwd: &Path, cmd: &str) {
    assert!(
        Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .status()
            .unwrap()
            .success(),
        "{cmd}"
    )
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn removes_only_its_own_temporary_files() {
    let root = std::env::temp_dir().join(format!("reclaim-cli-tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for name in [
        ".reclaim-tmp.99999.0",
        ".reclaim-tmp.99999.1",
        ".reclaim-tmp.99999.2",
        ".reclaim-tmp.99999.important",
    ] {
        std::fs::write(root.join(name), b"data").unwrap();
    }
    // Only .1 lacks the marker; .2 is excluded and .important does not have the generated form.
    sh(
        &root,
        "for f in .reclaim-tmp.99999.0 .reclaim-tmp.99999.2 .reclaim-tmp.99999.important; do xattr -w io.github.jonashaag.reclaim.tmp x $f; done",
    );

    run(
        &["--no-dedupe", "--no-compress", "-e", ".reclaim-tmp.99999.2"],
        &root,
    );
    assert_eq!(
        names(&root),
        [
            ".reclaim-tmp.99999.1",
            ".reclaim-tmp.99999.2",
            ".reclaim-tmp.99999.important"
        ]
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn leaves_files_alone_whose_blocks_files_outside_the_paths_share() {
    let base = std::env::temp_dir().join(format!("reclaim-cli-shared-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (root, outside) = (base.join("root"), base.join("outside"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let data = text(8 << 20, 7);
    std::fs::write(root.join("a.txt"), &data).unwrap();
    sh(
        &base,
        "cp -c root/a.txt root/b.txt && cp -c root/a.txt outside/c.txt",
    );
    // A partial change gives c.txt a data stream of its own that still shares most blocks, which
    // the clone reference count of a.txt and b.txt does not count.
    let c = std::fs::OpenOptions::new()
        .write(true)
        .open(outside.join("c.txt"))
        .unwrap();
    std::os::unix::fs::FileExt::write_all_at(&c, b"changed", 0).unwrap();
    drop(c);

    let out = Command::new(env!("CARGO_BIN_EXE_reclaim"))
        .args(["--no-cache"])
        .arg(&root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    for name in ["a.txt", "b.txt"] {
        assert!(!compressed(&root.join(name)), "{name}: {stdout}");
        assert_eq!(std::fs::read(root.join(name)).unwrap(), data, "{name}");
    }
    assert!(
        stdout.contains("skipped 2 times: files outside the given paths share its blocks"),
        "{stdout}"
    );
    assert!(stdout.contains("hint: also pass"), "{stdout}");
    assert!(!stdout.contains("stay allocated"), "{stdout}");
    assert_eq!(names(&root), ["a.txt", "b.txt"]);
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn does_not_warn_when_a_compressed_file_is_replaced_by_a_clone() {
    let base =
        std::env::temp_dir().join(format!("reclaim-cli-recompressed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    // Large enough that the compressed data goes to the resource fork instead of the decmpfs xattr.
    let data = text(1 << 20, 10);
    std::fs::write(base.join("a.txt"), &data).unwrap();
    std::fs::write(base.join("b.txt"), &data).unwrap();
    run(&["--no-dedupe"], &base);
    assert!(compressed(&base.join("a.txt")) && compressed(&base.join("b.txt")));

    let out = Command::new(env!("CARGO_BIN_EXE_reclaim"))
        .args(["--no-cache"])
        .arg(&base)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.contains("cloned 1 files"), "{stdout}");
    assert!(!stdout.contains("stay allocated"), "{stdout}");
    assert!(!stderr.contains("stay allocated"), "{stderr}");
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn counts_files_under_overlapping_paths_once() {
    let base = std::env::temp_dir().join(format!("reclaim-cli-overlap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (root, outside) = (base.join("root"), base.join("outside"));
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(root.join("sub/a.txt"), text(1 << 20, 8)).unwrap();
    sh(&base, "cp -c root/sub/a.txt outside/b.txt");

    let out = Command::new(env!("CARGO_BIN_EXE_reclaim"))
        .args(["--no-cache"])
        .args([&root, &root.join("sub")])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(!compressed(&root.join("sub/a.txt")), "{stdout}");
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn leaves_clones_alone_if_one_of_them_cannot_be_replaced() {
    let base = std::env::temp_dir().join(format!("reclaim-cli-partial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("a.txt"), text(1 << 20, 9)).unwrap();
    sh(
        &base,
        "cp -c a.txt b.txt && chmod +a 'everyone deny delete' b.txt",
    );

    let out = Command::new(env!("CARGO_BIN_EXE_reclaim"))
        .args(["--no-cache"])
        .arg(&base)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(!compressed(&base.join("a.txt")), "{stdout}");
    assert!(!compressed(&base.join("b.txt")), "{stdout}");
    sh(&base, "chmod -N b.txt");
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn frees_space_and_keeps_every_file_intact() {
    let root = fixture();
    let open = std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("a/open.txt"))
        .unwrap();
    let before = snapshot(&root, false);

    let dry = snapshot(&root, true);
    run(&["--dry-run"], &root);
    assert_eq!(snapshot(&root, true), dry, "a dry run changes nothing");

    let report = run(&[], &root);
    let mut expected = before.clone();
    expected.retain(|l| !l.starts_with("c/.reclaim-tmp."));
    assert_eq!(snapshot(&root, false), expected, "{report}");

    for p in [
        "a/source.py",
        "b/source.py",
        "c/deep/source.py",
        "a/big.txt",
        "b/big-clone.txt",
        "a/small.txt",
    ] {
        assert!(compressed(&root.join(p)), "{p} is compressed\n{report}");
    }
    for p in ["a/random.bin", "a/tiny.txt", "a/linked.txt", "a/open.txt"] {
        assert!(!compressed(&root.join(p)), "{p} is left alone\n{report}");
    }
    let ino = |p: &str| std::fs::symlink_metadata(root.join(p)).unwrap().ino();
    assert_eq!(
        ino("a/linked.txt"),
        ino("b/linked.txt"),
        "hard links stay hard links"
    );
    // b/random.bin, two copies of source.py and the clone of big.txt, which is compressed once for both.
    assert!(report.contains("cloned 4 files"), "{report}");

    let again = run(&[], &root);
    assert!(
        again.contains("cloned 0 files") && again.contains("compressed 0 files"),
        "{again}"
    );
    drop(open);
    std::fs::remove_dir_all(&root).unwrap();
}
