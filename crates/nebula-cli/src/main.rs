//! The `nebula` command-line tool.

use clap::Parser;

/// Peer-to-peer folder sync for your own devices. No cloud, no accounts.
#[derive(Parser)]
#[command(name = "nebula", version)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
