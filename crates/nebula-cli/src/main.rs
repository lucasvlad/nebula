//! The `nebula` command-line tool.

mod pair;
mod run;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

    /// Explain what Nebula is doing (-v), or every decision it makes (-vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

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
    },
    /// Show what Nebula knows about the files in a synced folder.
    Ls { folder: PathBuf },
    /// Pair with another device. Run with no code to show one, then run
    /// `nebula pair <code>` on the other device.
    Pair {
        /// The code shown on the other device.
        code: Option<String>,
        /// The other device's address, if discovery doesn't find it.
        #[arg(long, value_name = "HOST[:PORT]")]
        addr: Option<String>,
        /// Port to listen on while showing a code.
        #[arg(long, default_value_t = pair::PAIR_PORT)]
        port: u16,
    },
    /// Show or change the name other devices see for this one.
    Name { new_name: Option<String> },
    /// List paired devices.
    Peers,
    /// Remove a device from the paired list, by name or device ID.
    Unpair { device: String },
    /// Run Nebula: accept encrypted connections from paired devices.
    Run {
        #[arg(long, default_value_t = run::RUN_PORT)]
        port: u16,
    },
    /// Open an encrypted connection to a paired device and time a few round trips.
    Ping {
        /// Paired device name or ID (optional if there is only one).
        device: Option<String>,
        /// Connect to this address instead of looking the device up.
        #[arg(long, value_name = "HOST[:PORT]")]
        addr: Option<String>,
        /// How many pings to send.
        #[arg(short = 'c', long, default_value_t = 3)]
        count: u32,
    },
}

/// How many paths to print per category before summarising.
const LIST_LIMIT: usize = 10;

fn main() -> Result<()> {
    let cli = Cli::parse();
    // `run` is a long-lived process, so it narrates by default.
    let base = u8::from(matches!(cli.command, Command::Run { .. }));
    init_logging(cli.verbose.saturating_add(base));
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
        Command::Scan { folder } => {
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
                println!("Scanning {}", tilde(&f.root));
                let started = Instant::now();
                let report = scanner::scan(&mut store, &f, device)
                    .with_context(|| format!("scanning {}", f.root.display()))?;
                print_report(&report, started.elapsed(), cli.verbose > 0);
            }
        }
        Command::Ls { folder } => {
            let store = open_store(&home)?;
            let f = find_folder(&store, &folder)?;
            let device = identity(&home)?.device_id();
            let files = store.files(f.id)?;
            if files.is_empty() {
                println!("Nothing recorded yet. Run `nebula scan` first.");
                return Ok(());
            }
            println!(
                "{:>10}  {:>6}  {:<8}  {:<16}  PATH",
                "SIZE", "CHUNKS", "CONTENT", "VERSION"
            );
            for e in files {
                let version: Vec<String> = e
                    .version
                    .iter()
                    .map(|(d, n)| format!("{}:{n}", d.short()))
                    .collect();
                if e.deleted {
                    println!(
                        "{:>10}  {:>6}  {:<8}  {:<16}  {}",
                        "deleted",
                        "",
                        "",
                        version.join(" "),
                        e.path
                    );
                } else {
                    println!(
                        "{:>10}  {:>6}  {:<8}  {:<16}  {}",
                        human_bytes(e.size),
                        e.chunks.len(),
                        e.content_hash.short(),
                        version.join(" "),
                        e.path
                    );
                }
            }
            println!();
            println!(
                "VERSION lists each device that changed a file and how many times \
                 (this device is {}).",
                device.short()
            );
            println!("CONTENT is the start of the file's BLAKE3 hash: same hash, same bytes.");
        }
        Command::Pair { code, addr, port } => {
            let identity = identity(&home)?;
            let store = open_store(&home)?;
            let rt = tokio::runtime::Runtime::new()?;
            match code {
                Some(code) => rt.block_on(pair::join(&identity, &store, &code, addr.as_deref()))?,
                None => rt.block_on(pair::listen(&identity, &store, port))?,
            }
        }
        Command::Name { new_name } => {
            let store = open_store(&home)?;
            match new_name {
                Some(name) => {
                    let name = pair::set_device_name(&store, &name)?;
                    println!("This device is now called \"{name}\".");
                    println!(
                        "Devices you pair with from now on will see this name. \
                         Already-paired devices keep the old one until you pair again."
                    );
                }
                None => println!("{}", pair::device_name(&store)?),
            }
        }
        Command::Peers => pair::list(&open_store(&home)?)?,
        Command::Unpair { device } => pair::unpair(&open_store(&home)?, &device)?,
        Command::Run { port } => {
            let identity = identity(&home)?;
            let store = open_store(&home)?;
            tokio::runtime::Runtime::new()?.block_on(run::run(identity, store, port))?;
        }
        Command::Ping {
            device,
            addr,
            count,
        } => {
            let identity = identity(&home)?;
            let store = open_store(&home)?;
            tokio::runtime::Runtime::new()?.block_on(run::ping(
                identity,
                store,
                device.as_deref(),
                addr.as_deref(),
                count,
            ))?;
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

fn print_report(r: &ScanReport, took: Duration, show_all: bool) {
    let limit = if show_all { usize::MAX } else { LIST_LIMIT };
    print_list(
        "+ added   ",
        r.added.iter().map(|a| {
            let plural = if a.chunks == 1 { "" } else { "s" };
            format!(
                "{}  ({}, {} chunk{plural})",
                a.path,
                human_bytes(a.size),
                group(a.chunks as u64)
            )
        }),
        limit,
    );
    print_list(
        "~ modified",
        r.modified.iter().map(|m| {
            if m.chunks_new == m.chunks_total {
                return format!("{}  ({}, whole file changed)", m.path, human_bytes(m.size));
            }
            format!(
                "{}  ({} of {} chunks changed: only {} of {} needs sending)",
                m.path,
                group(m.chunks_new as u64),
                group(m.chunks_total as u64),
                human_bytes(m.new_bytes),
                human_bytes(m.size)
            )
        }),
        limit,
    );
    print_list(
        "- deleted ",
        r.deleted
            .iter()
            .map(|p| format!("{p}  (your other devices will delete it too)")),
        limit,
    );
    print_list(
        "! skipped ",
        r.skipped
            .iter()
            .map(|s| format!("{}  ({})", s.path.display(), s.reason)),
        limit,
    );
    if !r.has_changes() {
        println!("  No changes since the last scan.");
    }
    let plural = if r.read_files == 1 { "" } else { "s" };
    let mut detail = format!(
        "  Read {} file{plural} ({}) in {:.2} s",
        group(r.read_files as u64),
        human_bytes(r.read_bytes),
        took.as_secs_f64()
    );
    if r.trusted > 0 {
        detail += &format!(
            "; {} unchanged by size and modification time, so not re-read",
            group(r.trusted as u64)
        );
    }
    println!("{detail}.");
    println!(
        "  {} added, {} modified, {} deleted, {} unchanged, {} skipped.",
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
        println!(
            "  {label}  ... and {} more (use -v to list all)",
            total - limit
        );
    }
}

fn init_logging(verbose: u8) {
    let level = match verbose {
        0 => tracing::Level::WARN,
        1 => tracing::Level::INFO,
        _ => tracing::Level::DEBUG,
    };
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .init();
}

/// Shows paths under the user's home directory as `~/...`.
fn tilde(p: &Path) -> String {
    match directories::BaseDirs::new()
        .and_then(|b| p.strip_prefix(b.home_dir()).ok().map(Path::to_path_buf))
    {
        Some(rest) => format!("~/{}", rest.display()),
        None => p.display().to_string(),
    }
}

/// `12203` -> `12,203`.
fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(12_203), "12,203");
        assert_eq!(group(1_000_000), "1,000,000");
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(5), "5 B");
        assert_eq!(human_bytes(2_355), "2.3 KiB");
        assert_eq!(human_bytes(100_000_000), "95.4 MiB");
    }
}
