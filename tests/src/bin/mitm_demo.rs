//! Presentation demo: what an eavesdropper or a man in the middle sees and
//! can do, with and without Nebula's encryption and key pinning.
//!
//! This lives in the test package on purpose. The attacker code is never
//! part of the `nebula` binary.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use nebula_net::identity::Identity;
use nebula_net::transport::{self, TransportError};
use nebula_sync::protocol::Message;
use nebula_tests::demo::{
    Captured, RelayOpts, bold, contains_text, cyan, dim, green, hexdump, red, relay,
    report_readable, yellow,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const NOTE: &str = "Meet at the library at 7. My locker code is 4821.";

#[derive(Parser)]
#[command(
    name = "mitm-demo",
    about = "Eavesdropping and man-in-the-middle demonstrations"
)]
struct Cli {
    /// Do not wait for Enter between acts.
    #[arg(long, global = true)]
    no_pause: bool,
    #[command(subcommand)]
    mode: Option<Mode>,
}

#[derive(Subcommand)]
enum Mode {
    /// A four-act story, entirely in this process (default).
    Simulate,
    /// Sit between two real nebula processes and print what passes.
    Snoop {
        #[arg(long)]
        listen: String,
        #[arg(long)]
        to: String,
        /// Flip one bit of the byte at this offset of the client-to-server stream.
        #[arg(long)]
        tamper_at: Option<u64>,
        /// Report whether this text appears in the captured bytes (repeatable).
        #[arg(long)]
        look_for: Vec<String>,
    },
    /// Pretend to be the server to a real client, and the client to a real server.
    Impersonate {
        #[arg(long)]
        listen: String,
        #[arg(long)]
        to: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.mode.unwrap_or(Mode::Simulate) {
        Mode::Simulate => simulate(cli.no_pause).await,
        Mode::Snoop {
            listen,
            to,
            tamper_at,
            look_for,
        } => snoop(&listen, &to, tamper_at, look_for).await,
        Mode::Impersonate { listen, to } => impersonate(&listen, &to).await,
    }
}

// ---- shared bits ---------------------------------------------------------

fn header(title: &str) {
    println!();
    println!("{}", bold(&"=".repeat(70)));
    println!("{}", bold(title));
    println!("{}", bold(&"=".repeat(70)));
}

fn explain(text: &str) {
    println!("{text}");
    println!();
}

fn pause(no_pause: bool) {
    if no_pause {
        return;
    }
    println!();
    println!("{}", dim("Press Enter to continue..."));
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

async fn local_listener() -> Result<(TcpListener, SocketAddr)> {
    let l = TcpListener::bind("127.0.0.1:0").await?;
    let addr = l.local_addr()?;
    Ok((l, addr))
}

/// An eavesdropping relay: one connection, forwarded to `server`, recorded.
async fn eavesdropper(server: SocketAddr) -> Result<(SocketAddr, JoinHandle<Result<Captured>>)> {
    let (l, addr) = local_listener().await?;
    let task = tokio::spawn(async move {
        let (client, _) = l.accept().await?;
        let server = TcpStream::connect(server).await?;
        Ok(relay(client, server, &RelayOpts::default()).await)
    });
    Ok((addr, task))
}

fn show_capture(c: &Captured) {
    let all: Vec<u8> = [c.c2s.as_slice(), c.s2c.as_slice()].concat();
    println!(
        "{}",
        cyan(&format!(
            "The eavesdropper captured {} bytes. What it saw (Alice → Bob):",
            c.c2s.len()
        ))
    );
    for line in hexdump(&c.c2s, 96).lines() {
        println!("    {}", dim(line));
    }
    report_readable(&all);
}

fn code_words(c: impl std::fmt::Display) -> String {
    c.to_string()
}

fn pair_keys() -> Result<(Identity, Identity)> {
    Ok((Identity::generate()?, Identity::generate()?))
}

fn note_bytes(text: &str) -> Vec<u8> {
    Message::Note { text: text.into() }.encode()
}

fn note_text(bytes: &[u8]) -> Result<String> {
    match Message::decode(bytes)? {
        Message::Note { text } => Ok(text),
        other => bail!("unexpected message {other:?}"),
    }
}

// ---- simulate ------------------------------------------------------------

async fn simulate(no_pause: bool) -> Result<()> {
    println!(
        "{}",
        bold("Nebula security demo: can anyone read or change what we send?")
    );
    println!("Alice and Bob are two devices on the same Wi-Fi. Mallory is on that Wi-Fi too.");

    // Act 1
    header("Act 1: An app without encryption");
    explain(
        "Alice sends Bob a private message over a plain connection, the way many older\n\
         programs do. Eve, an eavesdropper, copies every byte as it passes. She does not\n\
         need to hack anything: on shared Wi-Fi, passing traffic can simply be recorded.",
    );
    let (bob_l, bob_addr) = local_listener().await?;
    let (eve_addr, eve) = eavesdropper(bob_addr).await?;
    let bob = tokio::spawn(async move {
        let (mut s, _) = bob_l.accept().await?;
        let mut v = Vec::new();
        s.read_to_end(&mut v).await?;
        anyhow::Ok(v)
    });
    let mut alice = TcpStream::connect(eve_addr).await?;
    println!("Alice sends: \"{NOTE}\"");
    alice.write_all(NOTE.as_bytes()).await?;
    alice.shutdown().await?;
    let got = bob.await??;
    let cap = eve.await??;
    println!("Bob received: \"{}\"", String::from_utf8_lossy(&got));
    show_capture(&cap);
    println!("{}", red("The eavesdropper read everything."));
    pause(no_pause);

    // Act 2
    header("Act 2: Nebula, with a listener on the network");
    explain(
        "Now Alice and Bob use Nebula. They have paired before, so each knows the other's\n\
         secret-handshake key. Their connection is encrypted from the first message. Eve\n\
         is still copying every byte, but what she copies is scrambled.",
    );
    let (alice_id, bob_id) = pair_keys()?;
    let (apk, bpk) = (*alice_id.public_key(), *bob_id.public_key());
    let (bob_l, bob_addr) = local_listener().await?;
    let (eve_addr, eve) = eavesdropper(bob_addr).await?;
    let bob = tokio::spawn(async move {
        let (s, _) = bob_l.accept().await?;
        let mut sess = transport::accept(s, &bob_id, |k| *k == apk).await?;
        let text = note_text(&sess.recv().await?)?;
        anyhow::Ok((sess.security_code(), text))
    });
    let s = TcpStream::connect(eve_addr).await?;
    let mut sess = transport::connect(s, &alice_id, |k| *k == bpk).await?;
    println!("Alice sends (encrypted): \"{NOTE}\"");
    sess.send(&note_bytes(NOTE)).await?;
    let alice_code = sess.security_code();
    drop(sess);
    let (bob_code, text) = bob.await??;
    let cap = eve.await??;
    println!("Bob decrypted: \"{}\"", green(&text));
    show_capture(&cap);
    let all: Vec<u8> = [cap.c2s.as_slice(), cap.s2c.as_slice()].concat();
    if contains_text(&all, "locker") || contains_text(&all, NOTE) {
        bail!("the message leaked through encryption: this must never happen");
    }
    println!(
        "  {}",
        green("The sentence and the word \"locker\" are not in there.")
    );
    println!(
        "Security codes: Alice sees {}, Bob sees {}",
        green(&code_words(alice_code)),
        green(&code_words(bob_code))
    );
    println!("{}", green("Identical: nobody is in the middle."));
    pause(no_pause);

    // Act 3
    header("Act 3: A middleman, if devices trusted anyone");
    explain(
        "Mallory now sits between Alice and Bob and plays Bob to Alice and Alice to Bob,\n\
         with two separate encrypted connections. WARNING: for this act only, Alice and\n\
         Bob are set up to accept ANY key. That is a deliberately broken setup, shown to\n\
         explain why Nebula pins keys. Nebula itself never does this.",
    );
    let (alice_id, bob_id) = pair_keys()?;
    let mallory = Identity::generate()?;
    let (bob_l, bob_addr) = local_listener().await?;
    let (mal_l, mal_addr) = local_listener().await?;
    let bob = tokio::spawn(async move {
        let (s, _) = bob_l.accept().await?;
        let mut sess = transport::accept(s, &bob_id, |_| true).await?;
        let text = note_text(&sess.recv().await?)?;
        anyhow::Ok((sess.security_code(), text))
    });
    let mal = tokio::spawn(async move {
        let (from_alice_tcp, _) = mal_l.accept().await?;
        let to_bob_tcp = TcpStream::connect(bob_addr).await?;
        let (a, b) = tokio::join!(
            transport::accept(from_alice_tcp, &mallory, |_| true),
            transport::connect(to_bob_tcp, &mallory, |_| true)
        );
        let (mut from_alice, mut to_bob) = (a?, b?);
        let read = note_text(&from_alice.recv().await?)?;
        let altered = read.replace("at 7", "at 9");
        to_bob.send(&note_bytes(&altered)).await?;
        anyhow::Ok((read, altered))
    });
    let s = TcpStream::connect(mal_addr).await?;
    let mut sess = transport::connect(s, &alice_id, |_| true).await?;
    println!("Alice sends (encrypted, she thinks to Bob): \"{NOTE}\"");
    sess.send(&note_bytes(NOTE)).await?;
    let alice_code = sess.security_code();
    let (read, altered) = mal.await??;
    let (bob_code, bob_text) = bob.await??;
    println!("{}", red(&format!("Mallory read: \"{read}\"")));
    println!(
        "{}",
        red(&format!(
            "Mallory changed it to: \"{altered}\" and passed it on."
        ))
    );
    println!("Bob received: \"{}\"", yellow(&bob_text));
    println!(
        "Security codes: Alice sees {}   Bob sees {}",
        red(&code_words(alice_code)),
        red(&code_words(bob_code))
    );
    println!("{}", red("They DIFFER."));
    println!("If Alice and Bob compare their four words, they catch the middleman.");
    pause(no_pause);

    // Act 4
    header("Act 4: The same middleman against real Nebula");
    explain(
        "Back to normal: Alice and Bob each accept only the key of the device they paired\n\
         with. Mallory tries exactly the same trick, with her own key.",
    );
    let (alice_id, bob_id) = pair_keys()?;
    let mallory = Identity::generate()?;
    let apk = *alice_id.public_key();
    let bpk = *bob_id.public_key();
    let (bob_l, bob_addr) = local_listener().await?;
    let (mal_l, mal_addr) = local_listener().await?;
    let bob = tokio::spawn(async move {
        let (s, _) = bob_l.accept().await?;
        anyhow::Ok(
            transport::accept(s, &bob_id, |k| *k == apk)
                .await
                .map(|_| ()),
        )
    });
    let mal = tokio::spawn(async move {
        let (from_alice_tcp, _) = mal_l.accept().await?;
        let to_bob_tcp = TcpStream::connect(bob_addr).await?;
        let (a, b) = tokio::join!(
            transport::accept(from_alice_tcp, &mallory, |_| true),
            transport::connect(to_bob_tcp, &mallory, |_| true)
        );
        let mut got = String::new();
        if let Ok(mut s) = a {
            got = format!("{:?}", s.recv().await.map(|v| v.len()));
        }
        if let Ok(mut s) = b {
            // Her side of the handshake finished, but Bob has hung up.
            let r = tokio::time::timeout(Duration::from_secs(3), s.recv()).await;
            if let Ok(Ok(v)) = r {
                got = format!("{} bytes from Bob", v.len());
            }
        }
        anyhow::Ok(got)
    });
    let s = TcpStream::connect(mal_addr).await?;
    let alice_res = transport::connect(s, &alice_id, |k| *k == bpk)
        .await
        .map(|_| ());
    let bob_res = bob.await??;
    let mal_got = mal.await??;
    match alice_res {
        Err(TransportError::NotAllowed(id)) => println!(
            "{}",
            green(&format!(
                "Alice refused: the device that answered (device {}) is not the one she paired with.",
                id.short()
            ))
        ),
        other => bail!("Alice should have refused, got {other:?}"),
    }
    match bob_res {
        Err(TransportError::NotAllowed(id)) => println!(
            "{}",
            green(&format!(
                "Bob refused: the device that connected (device {}) is not the one he paired with.",
                id.short()
            ))
        ),
        other => bail!("Bob should have refused, got {other:?}"),
    }
    if !mal_got.is_empty() {
        bail!("Mallory got something: {mal_got}");
    }
    println!("Mallory got no message from either side.");
    println!(
        "{}",
        green("Nebula stopped the middleman before a single word was sent.")
    );

    header("Summary");
    println!(
        "  Act 1  Plain connection        {}",
        red("Eavesdropper read everything")
    );
    println!(
        "  Act 2  Nebula, eavesdropper    {}",
        green("Only scrambled bytes; codes match")
    );
    println!(
        "  Act 3  Accept-anyone (broken)  {}",
        red("Middleman read and edited it; codes differ")
    );
    println!(
        "  Act 4  Nebula, middleman       {}",
        green("Both devices refused Mallory")
    );
    println!();
    Ok(())
}

// ---- snoop ---------------------------------------------------------------

async fn snoop(
    listen: &str,
    to: &str,
    tamper_at: Option<u64>,
    look_for: Vec<String>,
) -> Result<()> {
    explain_snoop(listen, to, tamper_at);
    let l = TcpListener::bind(listen).await?;
    println!(
        "{}",
        green(&format!(
            "Snooping on {listen}, forwarding to {to}. Ctrl+C to stop."
        ))
    );
    loop {
        let (client, peer) = l.accept().await?;
        let (to, look_for) = (to.to_string(), look_for.clone());
        tokio::spawn(async move {
            println!();
            println!("{}", bold(&format!("Connection from {peer}")));
            let server = match TcpStream::connect(&to).await {
                Ok(s) => s,
                Err(e) => {
                    println!(
                        "  {}",
                        red(&format!("Could not reach the real device at {to}: {e}"))
                    );
                    return;
                }
            };
            let opts = RelayOpts {
                live: true,
                tamper_at,
            };
            let cap = relay(client, server, &opts).await;
            println!("{}", bold("Connection closed. Summary:"));
            println!("  Device B → Device A: {} bytes", cap.c2s.len());
            println!("  Device A → Device B: {} bytes", cap.s2c.len());
            let all: Vec<u8> = [cap.c2s.as_slice(), cap.s2c.as_slice()].concat();
            nebula_tests::demo::report_readable(&all);
            for text in &look_for {
                if contains_text(&all, text) {
                    println!("  '{text}' — {}", red("FOUND in the captured bytes"));
                } else {
                    println!("  '{text}' — {}", green("not found"));
                }
            }
        });
    }
}

fn explain_snoop(listen: &str, to: &str, tamper_at: Option<u64>) {
    header("Snoop: a listener between two real Nebula devices");
    explain(&format!(
        "This program sits on the path between two real devices ({listen} → {to}),\n\
         copies every byte that passes, and prints it. It does not know any keys."
    ));
    if let Some(n) = tamper_at {
        explain(&format!(
            "It will also change one bit of byte {n} on the way through, like a faulty\n\
             cable or a meddling attacker. Nebula should notice and drop the connection."
        ));
    }
}

// ---- impersonate ---------------------------------------------------------

async fn impersonate(listen: &str, to: &str) -> Result<()> {
    header("Impersonate: Mallory pretends to be each device to the other");
    explain(&format!(
        "Mallory has her own key. She waits on {listen} for a real device to connect, and\n\
         answers as if she were the server. At the same moment she connects to the real\n\
         server at {to} and acts as the client. She accepts any key she is shown; the\n\
         question is whether the real devices accept hers. Handles one connection."
    ));
    let mallory = Identity::generate()?;
    println!(
        "Mallory's device ID: {}",
        yellow(&mallory.device_id().to_string())
    );
    let l = TcpListener::bind(listen).await?;
    println!("Waiting for a client on {listen}...");
    let (client, peer) = l.accept().await?;
    println!("Client connected from {peer}.");
    let server = TcpStream::connect(to).await?;
    let (a, b) = tokio::join!(
        transport::accept(client, &mallory, |_| true),
        transport::connect(server, &mallory, |_| true)
    );
    match a {
        Ok(s) => println!(
            "Client side: it accepted Mallory. It presented device {}. {}",
            s.remote_device().short(),
            red("Mallory could now read what it sends.")
        ),
        Err(e) => println!(
            "Client side: {} ({e})",
            green(
                "the real client refused: the device that answered is not the one it paired with."
            )
        ),
    }
    match b {
        Ok(mut s) => {
            let r = tokio::time::timeout(Duration::from_secs(3), s.recv()).await;
            match r {
                Ok(Ok(v)) => println!(
                    "Server side: {}",
                    red(&format!("it talked to Mallory ({} bytes)!", v.len()))
                ),
                _ => println!(
                    "Server side: {} Device {} is not paired, so it hung up.",
                    green("the real server refused."),
                    mallory.device_id()
                ),
            }
        }
        Err(e) => println!("Server side: {} ({e})", green("the real server refused.")),
    }
    println!(
        "{}",
        green("Mallory learned nothing and sent nothing useful.")
    );
    Ok(())
}
