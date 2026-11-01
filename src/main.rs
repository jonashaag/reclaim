//! Frees disk space on APFS. Replaces identical files with clones of one copy and compresses files
//! whose blocks no other file shares with APFS transparent compression.

mod cache;
mod compress;
mod dedupe;
mod replace;
mod run;
mod scan;
mod sys;

use clap::Parser;
use compress::Algorithm;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

/// Free disk space on APFS by cloning identical files and compressing the rest.
///
/// Identical files are replaced with APFS clones of one copy. Files that share no blocks with
/// other files are compressed with APFS transparent compression. Compressing one clone alone would
/// give it private blocks again, so clones are only compressed if all files sharing their blocks
/// are under PATHS. One compressed copy then replaces all of them. Modes, flags, extended
/// attributes and the creation, modification, access and added times are kept. Hard links, files
/// of other users and files that processes of this user have open or mapped for writing are
/// skipped.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Directories to process.
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Report what would be done without changing anything (still reads and compresses in memory).
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Do not replace identical files with clones.
    #[arg(long)]
    no_dedupe: bool,

    /// Do not compress files.
    #[arg(long)]
    no_compress: bool,

    /// Compression algorithm.
    #[arg(short, long, value_enum, default_value_t = Algorithm::Auto)]
    algorithm: Algorithm,

    /// Only clone or compress files of at least this size (suffixes k, m, g). Replacing a file
    /// takes about the same time whatever its size, so small files take most of the time but
    /// save little.
    #[arg(long, default_value = "16k", value_parser = parse_size)]
    min_size: u64,

    /// Only compress files whose allocated size shrinks by at least this many percent.
    #[arg(long, default_value_t = 10.0)]
    min_saving: f64,

    /// Skip directories and files with this name (repeatable).
    #[arg(short, long)]
    exclude: Vec<OsString>,

    /// Number of threads (default: number of CPUs).
    #[arg(short = 'j', long)]
    threads: Option<usize>,

    /// Make sure compressed data is on disk before it replaces a file, also against power loss
    /// (F_BARRIERFSYNC, about 1 ms per compressed file). By default it is only handed to the drive.
    #[arg(long)]
    durable: bool,

    /// Do not remember files that did not compress well enough (in ~/Library/Caches/reclaim).
    #[arg(long)]
    no_cache: bool,

    /// Print every file that could not be processed.
    #[arg(short, long)]
    verbose: bool,
}

fn parse_size(s: &str) -> Result<u64, String> {
    let (num, mult) = match s.to_ascii_lowercase().chars().last() {
        Some('k') => (&s[..s.len() - 1], 1 << 10),
        Some('m') => (&s[..s.len() - 1], 1 << 20),
        Some('g') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .map(|n| n * mult)
        .map_err(|e| e.to_string())
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

extern "C" fn on_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
    // A second Ctrl-C kills the process.
    unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
}

fn settings_fingerprint(args: &Args) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(
        &(
            env!("CARGO_PKG_VERSION"),
            args.algorithm as u8,
            args.min_saving.to_bits(),
        ),
        &mut h,
    );
    std::hash::Hasher::finish(&h)
}

fn size(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.2} GiB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64),
        b => format!("{:.0} KiB", b as f64 / 1024.0),
    }
}

/// Runs `work`. If stderr is a terminal, it shows the text that `status` returns in a line there
/// and updates it every 250 ms.
fn with_progress<T>(status: impl Fn() -> String + Sync, work: impl FnOnce() -> T) -> T {
    // Dropping `done` wakes the progress thread at once.
    let (done, wait) = std::sync::mpsc::channel::<()>();
    std::thread::scope(|s| {
        if std::io::stderr().is_terminal() {
            let status = &status;
            s.spawn(move || {
                let start = Instant::now();
                while let Err(RecvTimeoutError::Timeout) =
                    wait.recv_timeout(Duration::from_millis(250))
                {
                    eprint!("\r\x1b[K{:.0?}: {}", start.elapsed(), status());
                }
                eprint!("\r\x1b[K");
                let _ = std::io::stderr().flush();
            });
        }
        let result = work();
        drop(done);
        result
    })
}

fn main() {
    let args = Args::parse();
    if let Some(n) = args.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
            .unwrap();
    }
    unsafe { libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t) };

    let free_before = sys::free_space(&args.paths[0]).unwrap_or(0);
    let t0 = Instant::now();
    let open_for_writing = sys::files_open_for_writing();
    if open_for_writing.uninspected > 0 {
        eprintln!(
            "warning: could not list the open files of {} processes of this user; files they write to are not skipped",
            open_for_writing.uninspected
        );
    }
    let get = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
    let st = scan::ScanStats::default();
    let scan = with_progress(
        || {
            format!(
                "scanning, {} files in {} directories so far",
                get(&st.regular_files),
                get(&st.dirs)
            )
        },
        || {
            scan::scan(
                &args.paths,
                &scan::Options {
                    exclude: &args.exclude,
                    open_for_writing: &open_for_writing.files,
                    min_size: args.min_size.max(1),
                    dry_run: args.dry_run,
                },
                &st,
            )
        },
    );
    println!(
        "scanned {} files in {} directories in {:.1?}: {} logical, {} allocated, {} not shared",
        get(&st.regular_files),
        get(&st.dirs),
        t0.elapsed(),
        size(get(&st.logical_bytes)),
        size(get(&st.allocated_bytes)),
        size(get(&st.unshared_bytes)),
    );
    for (n, what) in [
        (get(&st.unreadable_dirs), "unreadable directories"),
        (
            get(&st.skipped_files),
            "hard links, files of other users or with protective flags",
        ),
        (get(&st.skipped_open), "files open or mapped for writing"),
    ] {
        if n > 0 {
            println!("  skipped {n} {what}");
        }
    }

    let opts = run::Options {
        dry_run: args.dry_run,
        dedupe: !args.no_dedupe,
        compress: !args.no_compress,
        min_size: args.min_size,
        verbose: args.verbose,
        compression: compress::Settings {
            algorithm: args.algorithm,
            dry_run: args.dry_run,
            min_saving: args.min_saving / 100.0,
            durable: args.durable,
        },
    };
    let incompressible = cache::Incompressible::load(
        if args.no_cache {
            None
        } else {
            cache::default_path()
        },
        settings_fingerprint(&args),
    );
    let runner = run::Runner::new(&scan, &opts, incompressible);
    let t1 = Instant::now();
    let hashed = &runner.stats.hashed;
    let plan = with_progress(
        || {
            format!(
                "comparing files of the same size, {} read ({})",
                get(&hashed.files),
                size(get(&hashed.bytes))
            )
        },
        || runner.plan(),
    );
    println!(
        "planned in {:.1?}: {} files with identical content, {} groups and files to compress",
        t1.elapsed(),
        plan.duplicates,
        plan.units()
    );

    let t2 = Instant::now();
    let stats = &runner.stats;
    with_progress(
        || {
            format!(
                "{}/{} done, {} freed",
                get(&stats.units_done),
                plan.units(),
                size(stats.freed())
            )
        },
        || runner.execute(&plan),
    );
    let s = runner.finish();
    let verb = if args.dry_run { "would have " } else { "" };
    println!(
        "{verb}cloned {} files ({}), freeing about {}",
        get(&s.cloned_files),
        size(get(&s.cloned_bytes)),
        size(get(&s.dedupe_freed))
    );
    println!(
        "{verb}compressed {} files from {} to {}; {} files ({}) did not compress well enough",
        get(&s.compressed_files),
        size(get(&s.compressed_before)),
        size(get(&s.compressed_after)),
        get(&s.incompressible_files),
        size(get(&s.incompressible_bytes)),
    );
    let mut skipped: Vec<_> = s
        .skipped
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| (*v, k.clone()))
        .collect();
    skipped.sort_unstable_by(|a, b| b.cmp(a));
    for (n, reason) in &skipped {
        println!("  skipped {n} times: {reason}");
    }
    if skipped.iter().any(|(_, r)| r == run::SHARED_OUTSIDE) {
        println!(
            "  hint: also pass the directories these files were cloned from, such as package caches (e.g. ~/Library/Caches/rattler for pixi, ~/Library/pnpm/store for pnpm)"
        );
    }
    let kept = get(&s.kept_changed);
    if kept > 0 {
        println!(
            "warning: {kept} files changed while being replaced; the changed versions are kept as *{}",
            replace::CHANGED_SUFFIX
        );
    }
    let shared_files = get(&s.still_shared_files);
    if shared_files > 0 {
        println!(
            "warning: {} of {shared_files} replaced files stay allocated, because files outside the given paths share their blocks",
            size(get(&s.still_shared))
        );
    }
    let partial = get(&s.partial_groups);
    if partial > 0 {
        println!(
            "warning: of {partial} groups of identical files only some files could be replaced; the blocks of the others stay allocated"
        );
    }
    let free_after = sys::free_space(&args.paths[0]).unwrap_or(0);
    println!(
        "done in {:.1?} ({:.1?} total); free space {} -> {} ({:+.2} GiB)",
        t2.elapsed(),
        t0.elapsed(),
        size(free_before),
        size(free_after),
        (free_after as f64 - free_before as f64) / (1u64 << 30) as f64
    );
    if interrupted() {
        println!("interrupted");
        std::process::exit(130);
    }
}
