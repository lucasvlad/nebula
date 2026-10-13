//! The `nebula` command-line tool.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use nebula_net::identity::Identity;
use nebula_sync::scanner::{self, ScanReport};
use nebula_sync::store::{Folder, Store};

/// Peer-to-peer folder sync for your own devices. No cloud, no accounts.
#[derive(Parser)]
#[command(name = "nebula", version)]
struct Cli {
    /// Where Nebula keeps this device's key and state.
    #[arg(long, global = true, env = "NEBULA_HOME", value_name = "DIR")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show this device's ID.
    Id,
    /// Start syncing a folder.
    Add { folder: PathBuf },
    /// Stop syncing a folder. Its files are left untouched.
    Remove { folder: PathBuf },
    /// List synced folders.
    Folders,
    /// Look for changes in synced folders and record them.
    Scan {
        /// Only this folder (default: all of them).
        folder: Option<PathBuf>,
        /// List every changed file instead of the first few.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Show what Nebula knows about the files in a synced folder.
    Ls { folder: PathBuf },
}

/// How many paths to print per category before summarising.
const LIST_LIMIT: usize = 10;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = match cli.home {
        Some(home) => home,
        None => default_home()?,
    };
    match cli.command {
        Command::Id => {
            let id = identity(&home)?;
            println!("{}", id.device_id());
        }
        Command::Add { folder } => add(&home, &folder)?,
        Command::Remove { folder } => {
            let store = open_store(&home)?;
            let root = std::fs::canonicalize(&folder).unwrap_or(folder);
            if store.remove_folder(&root)? {
                println!(
                    "Stopped syncing {}. Its files were not touched.",
                    root.display()
                );
            } else {
                bail!("{} is not a synced folder", root.display());
            }
        }
        Command::Folders => {
            let folders = open_store(&home)?.folders()?;
            if folders.is_empty() {
                println!("No synced folders. Add one with `nebula add <folder>`.");
            }
            for f in folders {
                let state = if f.last_scan_ns == 0 {
                    "  (not scanned yet)"
                } else {
                    ""
                };
                println!("{}{state}", f.root.display());
            }
        }
        Command::Scan { folder, verbose } => {
            let device = identity(&home)?.device_id();
            let mut store = open_store(&home)?;
            let folders = match folder {
                Some(f) => vec![find_folder(&store, &f)?],
                None => store.folders()?,
            };
            if folders.is_empty() {
                println!("No synced folders. Add one with `nebula add <folder>`.");
            }
            for f in folders {
                let report = scanner::scan(&mut store, &f, device)
                    .with_context(|| format!("scanning {}", f.root.display()))?;
                print_report(&f, &report, verbose);
            }
        }
        Command::Ls { folder } => {
            let store = open_store(&home)?;
            let f = find_folder(&store, &folder)?;
            for e in store.files(f.id)? {
                let version: Vec<String> = e
                    .version
                    .iter()
                    .map(|(d, n)| format!("{}:{n}", d.short()))
                    .collect();
                let state = if e.deleted {
                    "deleted".to_string()
                } else {
                    format!("{:>10}  {:>5} chunks", human_bytes(e.size), e.chunks.len())
                };
                println!(
                    "{:<28}  {}  [{}]  {}",
                    state,
                    e.content_hash.short(),
                    version.join(" "),
                    e.path
                );
            }
        }
    }
    Ok(())
}

fn default_home() -> Result<PathBuf> {
    directories::ProjectDirs::from("", "", "nebula")
        .map(|d| d.data_dir().to_path_buf())
        .context("could not find a home directory; pass --home")
}

fn identity(home: &Path) -> Result<Identity> {
    let path = home.join("identity.key");
    Identity::load_or_create(&path).with_context(|| format!("loading {}", path.display()))
}

fn open_store(home: &Path) -> Result<Store> {
    std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
    let path = home.join("state.db");
    Store::open(&path).with_context(|| format!("opening {}", path.display()))
}

fn add(home: &Path, folder: &Path) -> Result<()> {
    let root = std::fs::canonicalize(folder)
        .with_context(|| format!("{} does not exist", folder.display()))?;
    if !root.is_dir() {
        bail!("{} is not a folder", root.display());
    }
    let store = open_store(home)?;
    // Nebula's own state must not sync, or every write would trigger a sync.
    let home = std::fs::canonicalize(home)?;
    if home.starts_with(&root) {
        bail!(
            "{} contains Nebula's own data folder ({})",
            root.display(),
            home.display()
        );
    }
    for other in store.folders()? {
        if root.starts_with(&other.root) || other.root.starts_with(&root) {
            bail!(
                "{} overlaps the synced folder {}",
                root.display(),
                other.root.display()
            );
        }
    }
    store.add_folder(&root)?;
    println!(
        "Now syncing {}. Run `nebula scan` to index it.",
        root.display()
    );
    Ok(())
}

fn find_folder(store: &Store, folder: &Path) -> Result<Folder> {
    let root = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    store
        .folder(&root)?
        .with_context(|| format!("{} is not a synced folder", root.display()))
}

fn print_report(folder: &Folder, r: &ScanReport, verbose: bool) {
    println!("{}", folder.root.display());
    let limit = if verbose { usize::MAX } else { LIST_LIMIT };
    print_list("+ added   ", r.added.iter().map(|p| p.to_string()), limit);
    print_list(
        "~ modified",
        r.modified.iter().map(|m| {
            format!(
                "{}  ({} of {} chunks changed: {} to send of {})",
                m.path,
                m.chunks_new,
                m.chunks_total,
                human_bytes(m.new_bytes),
                human_bytes(m.size)
            )
        }),
        limit,
    );
    print_list("- deleted ", r.deleted.iter().map(|p| p.to_string()), limit);
    print_list(
        "! skipped ",
        r.skipped
            .iter()
            .map(|s| format!("{}: {}", s.path.display(), s.reason)),
        limit,
    );
    println!(
        "  {} added, {} modified, {} deleted, {} unchanged, {} skipped",
        r.added.len(),
        r.modified.len(),
        r.deleted.len(),
        r.unchanged,
        r.skipped.len()
    );
}

fn print_list(label: &str, items: impl ExactSizeIterator<Item = String>, limit: usize) {
    let total = items.len();
    for item in items.take(limit) {
        println!("  {label}  {item}");
    }
    if total > limit {
        println!("  {label}  ... and {} more (use --verbose)", total - limit);
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
