//! The `nebula` command-line tool.

mod pair;
mod run;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use nebula_node::{Node, NodeConfig, default_home, identity, open_store};
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
    Add {
        folder: PathBuf,
        /// Name to sync it under; other devices sync their folder with the
        /// same name. Defaults to the folder's own name.
        #[arg(long = "as", value_name = "NAME")]
        label: Option<String>,
    },
    /// Stop syncing a folder, by path or name. Its files are left untouched.
    Remove {
        #[arg(value_name = "PATH|NAME")]
        folder: PathBuf,
    },
    /// List synced folders.
    Folders,
    /// Look for changes in synced folders and record them.
    Scan {
        /// Only this folder, by path or name (default: all of them).
        #[arg(value_name = "PATH|NAME")]
        folder: Option<PathBuf>,
    },
    /// Show what Nebula knows about the files in a synced folder.
    Ls {
        /// The folder's path or name.
        #[arg(value_name = "PATH|NAME")]
        folder: PathBuf,
    },
    /// Pair with another device. Run with no code to show one, then run
    /// `nebula pair <code>` on the other device.
    Pair {
        /// The code shown on the other device.
        code: Option<String>,
        /// The other device's address, if discovery doesn't find it.
        #[arg(long, value_name = "HOST[:PORT]")]
        addr: Option<String>,
        /// Port to listen on while showing a code.
        #[arg(long, default_value_t = nebula_node::net::PAIR_PORT)]
        port: u16,
    },
    /// Show or change the name other devices see for this one.
    Name { new_name: Option<String> },
    /// List paired devices.
    Peers,
    /// Remove a device from the paired list, by name or device ID.
    Unpair { device: String },
    /// Run Nebula: stay connected to paired devices and sync folders with them.
    Run {
        #[arg(long, default_value_t = nebula_node::net::RUN_PORT)]
        port: u16,
        /// Also connect to a paired device at this address (repeatable), for
        /// networks where devices cannot find each other automatically.
        #[arg(long = "connect", value_name = "HOST[:PORT]")]
        connect: Vec<String>,
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
        /// Also send this text, encrypted, for the other device to display.
        #[arg(short, long)]
        message: Option<String>,
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
        Command::Add { folder, label } => add(&home, &folder, label.as_deref())?,
        Command::Remove { folder } => {
            let store = open_store(&home)?;
            let f = find_folder(&store, &folder)?;
            if store.remove_folder(&f.root)? {
                println!(
                    "Stopped syncing {} (\"{}\"). Its files were not touched.",
                    f.root.display(),
                    f.label
                );
            } else {
                bail!("{} is not a synced folder", f.root.display());
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
                println!("{:<20}  {}{state}", f.label, f.root.display());
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
            // One missing folder (an unplugged drive) must not stop the rest
            // from being scanned.
            let mut failed = 0;
            for f in folders {
                println!("Scanning {}", tilde(&f.root));
                let started = Instant::now();
                match scanner::scan(&mut store, &f, device) {
                    Ok(report) => print_report(&report, started.elapsed(), cli.verbose > 0),
                    Err(e) => {
                        eprintln!("Error: could not scan {}: {e}", f.root.display());
                        failed += 1;
                    }
                }
            }
            if failed > 0 {
                bail!(
                    "{failed} folder{} could not be scanned",
                    if failed == 1 { "" } else { "s" }
                );
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
                "{:>10}  {:>6}  {:<8}  {:<28}  PATH",
                "SIZE", "CHUNKS", "CONTENT", "VERSION"
            );
            for e in files {
                let version: Vec<String> = e
                    .version
                    .iter()
                    .map(|(d, n)| version_stamp(d, *n))
                    .collect();
                if e.deleted {
                    println!(
                        "{:>10}  {:>6}  {:<8}  {:<28}  {}",
                        "deleted",
                        "",
                        "",
                        version.join(" "),
                        e.path
                    );
                } else {
                    println!(
                        "{:>10}  {:>6}  {:<8}  {:<28}  {}",
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
                "VERSION lists each device that changed a file and when it last did, in UTC \
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
                    let name = nebula_node::net::set_device_name(&store, &name)?;
                    println!("This device is now called \"{name}\".");
                    println!(
                        "Devices you pair with from now on will see this name. \
                         Already-paired devices keep the old one until you pair again."
                    );
                }
                None => println!("{}", nebula_node::net::device_name(&store)?),
            }
        }
        Command::Peers => pair::list(&open_store(&home)?)?,
        Command::Unpair { device } => pair::unpair(&open_store(&home)?, &device)?,
        Command::Run { port, connect } => {
            tokio::runtime::Runtime::new()?.block_on(serve(home, port, connect))?;
        }
        Command::Ping {
            device,
            addr,
            count,
            message,
        } => {
            let identity = identity(&home)?;
            let store = open_store(&home)?;
            tokio::runtime::Runtime::new()?.block_on(run::ping(
                identity,
                store,
                device.as_deref(),
                addr.as_deref(),
                count,
                message.as_deref(),
            ))?;
        }
    }
    Ok(())
}

fn add(home: &Path, folder: &Path, label: Option<&str>) -> Result<()> {
    let store = open_store(home)?;
    let f = nebula_node::add_folder(&store, home, folder, label)?;
    println!(
        "Now syncing {} as \"{}\". On your other devices, add a folder with \
         the same name (`nebula add <folder> --as \"{}\"`) to sync them.",
        f.root.display(),
        f.label,
        f.label
    );
    Ok(())
}

/// `nebula run`: the same node the app runs, narrated to the terminal.
async fn serve(home: PathBuf, port: u16, connect: Vec<String>) -> Result<()> {
    let mut node = Node::start(NodeConfig {
        home,
        port,
        connect,
    })
    .await?;
    let status = node.status()?;
    let peers = status.peers.len();
    println!(
        "Nebula is running as \"{}\" (device {}) on port {}, with {peers} paired device{}.",
        status.me.name,
        status.me.id,
        node.port(),
        if peers == 1 { "" } else { "s" }
    );
    if let Some(ip) = status.me.addresses.first() {
        println!("Other devices can reach it at {ip}:{}.", node.port());
    }
    println!("Press Ctrl+C to stop.");
    let failed = tokio::select! {
        _ = tokio::signal::ctrl_c() => None,
        r = node.stopped() => Some(r),
    };
    match failed {
        Some(result) => result,
        None => {
            println!("Stopping.");
            node.shutdown().await
        }
    }
}

/// Finds a synced folder by name, or by path. The path may be relative, and
/// the folder may no longer exist (it was deleted, or is on an unplugged
/// drive), so it cannot simply be canonicalised.
fn find_folder(store: &Store, folder: &Path) -> Result<Folder> {
    if let Some(f) = folder
        .to_str()
        .map(|l| store.folder_by_label(l))
        .transpose()?
        .flatten()
    {
        return Ok(f);
    }
    if let Ok(root) = std::fs::canonicalize(folder)
        && let Some(f) = store.folder(&root)?
    {
        return Ok(f);
    }
    let absolute = std::path::absolute(folder).unwrap_or_else(|_| folder.to_path_buf());
    let mut wanted = vec![absolute.clone()];
    wanted.extend(canonical_prefix(&absolute));
    let wanted: Vec<PathBuf> = wanted.iter().map(|p| strip_verbatim(p)).collect();
    for f in store.folders()? {
        if wanted.contains(&strip_verbatim(&f.root)) {
            return Ok(f);
        }
    }
    bail!(
        "{} is not a synced folder (see `nebula folders`)",
        folder.display()
    )
}

/// `path` with its deepest existing ancestor canonicalised, so a deleted
/// folder under a symlinked parent (macOS `/var` is `/private/var`) still
/// matches the canonical root `nebula add` stored.
fn canonical_prefix(path: &Path) -> Option<PathBuf> {
    let mut rest = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(mut base) = std::fs::canonicalize(cur) {
            base.extend(rest.iter().rev());
            return Some(base);
        }
        rest.push(cur.file_name()?);
        cur = cur.parent()?;
    }
}

/// Drops Windows' `\\?\` prefix, which `canonicalize` adds and
/// `absolute` does not, so the two spellings compare equal.
fn strip_verbatim(p: &Path) -> PathBuf {
    match p.to_str() {
        Some(s) => match s.strip_prefix(r"\\?\UNC\") {
            Some(unc) => PathBuf::from(format!(r"\\{unc}")),
            None => PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(s)),
        },
        None => p.to_path_buf(),
    }
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

/// One version-vector entry for display. Counters are clock-stamped
/// (`VersionVector::bump`), so most read as times; older ones are counts.
fn version_stamp(device: &nebula_core::DeviceId, n: u64) -> String {
    match i64::try_from(n) {
        // After 2001-09-09: a nanosecond timestamp, not a small count.
        Ok(ns) if ns >= 1_000_000_000_000_000_000 => {
            let t = nebula_core::conflict::format_utc(ns);
            format!("{}@{}:{}", device.short(), &t[..13], &t[13..15])
        }
        _ => format!("{}:{n}", device.short()),
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
    fn strips_windows_verbatim_prefix() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\Users\me\docs")),
            Path::new(r"C:\Users\me\docs")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\server\share")),
            Path::new(r"\\server\share")
        );
        assert_eq!(strip_verbatim(Path::new("/home/me")), Path::new("/home/me"));
    }

    #[test]
    fn canonical_prefix_of_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone").join("deeper");
        let base = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(
            canonical_prefix(&gone),
            Some(base.join("gone").join("deeper"))
        );
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(5), "5 B");
        assert_eq!(human_bytes(2_355), "2.3 KiB");
        assert_eq!(human_bytes(100_000_000), "95.4 MiB");
    }
}
