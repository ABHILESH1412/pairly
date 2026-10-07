//! `pairly`: command-line client for `pairlyd`, talking over D-Bus.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use pairly_dbus::{DaemonProxy, Device, Transfer};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Parser)]
#[command(version, about = "Control the Pairly daemon")]
struct Args {
    /// Bus name of the daemon to talk to (for running several daemons).
    #[arg(long, global = true, default_value = pairly_dbus::BUS_NAME)]
    bus_name: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show this device's id and name.
    Id,
    /// List paired and discovered devices.
    Devices,
    /// Pair with a discovered device (by id, id prefix or name).
    Pair {
        device: String,
        /// Accept without asking. Testing only: skips the code comparison that stops MITM attacks.
        #[arg(long)]
        yes: bool,
    },
    /// Wait for incoming pairing requests and pings (Ctrl-C to stop).
    Listen {
        /// Accept every pairing request without asking. Testing only.
        #[arg(long)]
        yes: bool,
    },
    /// Send a ping, optionally with a message.
    Ping {
        device: String,
        message: Option<String>,
    },
    /// Make a device ring loudly to find it (`--stop` to stop).
    Ring {
        device: String,
        #[arg(long)]
        stop: bool,
    },
    /// Send this PC's clipboard text to a device.
    Clip { device: String },
    /// Send files to a device and show progress until they arrive.
    Send {
        device: String,
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Hand the files to the daemon and return right away.
        #[arg(long)]
        no_wait: bool,
    },
    /// Open a link on a device.
    Url { device: String, url: String },
    /// Copy text to a device's clipboard (with a notification there).
    Text { device: String, text: String },
    /// List file transfers in progress.
    Transfers,
    /// Forget a paired device.
    Unpair { device: String },
    /// Pause a paired device: nothing passes either way until it's resumed.
    Pause { device: String },
    /// Resume a paused device.
    Resume { device: String },
    /// Rename this PC (paired devices see it when they reconnect).
    Rename { name: String },
}

const PAIRING_TIMEOUT: Duration = Duration::from_secs(150);

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let conn = zbus::Connection::session()
        .await
        .context("connecting to the session bus")?;
    let daemon = DaemonProxy::builder(&conn)
        .destination(args.bus_name.clone())?
        .build()
        .await?;
    let not_running = || format!("is pairlyd running (bus name {})?", args.bus_name);

    match args.command {
        Command::Id => {
            let (id, name) = daemon.get_identity().await.with_context(not_running)?;
            println!("{name}\n{id}");
        }
        Command::Rename { name } => {
            daemon.set_name(&name).await.with_context(not_running)?;
            println!(
                "Renamed to {:?}. The daemon restarts to announce it.",
                name.trim()
            );
        }
        Command::Devices => {
            let devices = daemon.list_devices().await.with_context(not_running)?;
            print_devices(&devices);
        }
        Command::Pair { device, yes } => pair(&daemon, &device, yes).await?,
        Command::Listen { yes } => listen(&daemon, yes).await?,
        Command::Ping { device, message } => {
            let device = resolve(&daemon, &device).await?;
            daemon
                .ping(&device.id, message.as_deref().unwrap_or(""))
                .await?;
            println!("Pinged {}", device.name);
        }
        Command::Ring { device, stop } => {
            let device = resolve(&daemon, &device).await?;
            daemon.ring(&device.id, !stop).await?;
            println!(
                "{} {}",
                if stop { "Stopped ringing" } else { "Ringing" },
                device.name
            );
        }
        Command::Clip { device } => {
            let device = resolve(&daemon, &device).await?;
            daemon.send_clipboard(&device.id).await?;
            println!("Sent the clipboard to {}", device.name);
        }
        Command::Send {
            device,
            files,
            no_wait,
        } => {
            let device = resolve(&daemon, &device).await?;
            send(&daemon, &device, &files, no_wait).await?;
        }
        Command::Url { device, url } => {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                bail!(
                    "only http:// and https:// links can be opened; use `pairly text` for others"
                );
            }
            let device = resolve(&daemon, &device).await?;
            daemon.send_text(&device.id, &url, true).await?;
            println!("Sent the link to {}", device.name);
        }
        Command::Text { device, text } => {
            let device = resolve(&daemon, &device).await?;
            daemon.send_text(&device.id, &text, false).await?;
            println!("Sent the text to {}", device.name);
        }
        Command::Transfers => {
            let transfers = daemon.list_transfers().await.with_context(not_running)?;
            if transfers.is_empty() {
                println!("No transfers in progress.");
            }
            for t in transfers {
                let dir = if t.incoming { "from" } else { "to" };
                println!(
                    "{:<20}  {}  {dir} {}  {}/{}  {}",
                    t.id,
                    t.name,
                    t.device_name,
                    human_size(t.bytes),
                    human_size(t.size),
                    t.state
                );
            }
        }
        Command::Pause { device } => {
            let device = resolve(&daemon, &device).await?;
            daemon.set_paused(&device.id, true).await?;
            println!(
                "Paused {}: nothing passes either way until you resume it.",
                device.name
            );
        }
        Command::Resume { device } => {
            let device = resolve(&daemon, &device).await?;
            daemon.set_paused(&device.id, false).await?;
            println!("Resumed {}", device.name);
        }
        Command::Unpair { device } => {
            let device = resolve(&daemon, &device).await?;
            daemon.unpair(&device.id).await?;
            println!("Unpaired {}", device.name);
        }
    }
    Ok(())
}

fn print_devices(devices: &[Device]) {
    if devices.is_empty() {
        println!("No devices found yet.");
        return;
    }
    println!("{:<26}  {:<20}  {:<8}  STATUS", "ID", "NAME", "TYPE");
    for d in devices {
        let battery = if d.battery >= 0 {
            format!(
                ", battery {}%{}",
                d.battery,
                if d.charging { " charging" } else { "" }
            )
        } else {
            String::new()
        };
        let status = match (d.paired, d.is_connected()) {
            (true, true) if d.rtt_ms > 0 => format!("connected ({}, {} ms)", d.link, d.rtt_ms),
            (true, true) => format!("connected ({})", d.link),
            (true, _) if d.paused => "paused".to_owned(),
            (true, false) => "paired, offline".to_owned(),
            (false, _) => "available to pair".to_owned(),
        };
        let kind = if d.device_type.is_empty() {
            "-"
        } else {
            &d.device_type
        };
        println!(
            "{:<26}  {:<20}  {:<8}  {status}{battery}",
            d.id,
            truncate(&d.name, 20),
            kind
        );
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    #[allow(clippy::cast_precision_loss)] // display only
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Offer the files, then follow their progress until each one finishes.
async fn send(
    daemon: &DaemonProxy<'_>,
    device: &Device,
    files: &[PathBuf],
    no_wait: bool,
) -> Result<()> {
    let paths = files
        .iter()
        .map(|f| {
            std::path::absolute(f)
                .map(|p| p.to_string_lossy().into_owned())
                .with_context(|| format!("resolving {}", f.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    // Subscribe first so no update is missed.
    let mut changes = daemon.receive_transfer_changed().await?;
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let ids = daemon.send_files(&device.id, &refs).await?;
    println!("Offered {} file(s) to {}", ids.len(), device.name);
    if no_wait {
        return Ok(());
    }

    let tty = std::io::stdout().is_terminal();
    let mut pending: HashMap<u64, Transfer> = HashMap::new();
    let mut failed = 0;
    let mut remaining = ids.len();
    while remaining > 0 {
        let Some(signal) = changes.next().await else {
            bail!("lost the connection to pairlyd");
        };
        let t = signal.args()?.transfer;
        if !ids.contains(&t.id) {
            continue;
        }
        if t.is_finished() {
            remaining -= 1;
            pending.remove(&t.id);
            if tty {
                print!("\r\x1b[K");
            }
            match t.state.as_str() {
                "done" => println!("Sent {} ({})", t.name, human_size(t.size)),
                "failed" => {
                    failed += 1;
                    println!("Failed {}: {}", t.name, t.error);
                }
                _ => {
                    failed += 1;
                    println!("Cancelled {}", t.name);
                }
            }
        } else {
            if tty && t.state == "running" {
                print!(
                    "\r\x1b[K{}  {:>3.0}%  {} / {}",
                    t.name,
                    t.fraction() * 100.0,
                    human_size(t.bytes),
                    human_size(t.size)
                );
                std::io::stdout().flush()?;
            } else if t.state == "waiting" && !pending.contains_key(&t.id) {
                println!("Waiting for {} to accept {}...", device.name, t.name);
            }
            pending.insert(t.id, t);
        }
    }
    if failed > 0 {
        bail!("{failed} file(s) were not sent");
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max - 1).chain(['…']).collect()
    }
}

/// Find a device by exact id, unique id prefix (4+ chars) or case-insensitive name.
async fn resolve(daemon: &DaemonProxy<'_>, query: &str) -> Result<Device> {
    let devices = daemon.list_devices().await?;
    if let Some(d) = devices.iter().find(|d| d.id == query) {
        return Ok(d.clone());
    }
    let q = query.to_lowercase();
    let mut matches: Vec<&Device> = devices
        .iter()
        .filter(|d| (q.len() >= 4 && d.id.starts_with(&q)) || d.name.to_lowercase() == q)
        .collect();
    match matches.len() {
        1 => Ok(matches.remove(0).clone()),
        0 => bail!("no device matches {query:?}; see `pairly devices`"),
        _ => bail!("{query:?} matches several devices; use the id"),
    }
}

async fn ask(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    BufReader::new(tokio::io::stdin())
        .read_line(&mut line)
        .await?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

async fn pair(daemon: &DaemonProxy<'_>, query: &str, yes: bool) -> Result<()> {
    let device = resolve(daemon, query).await?;
    // Subscribe before starting, so no signal is missed.
    let mut requested = daemon.receive_pairing_requested().await?;
    let mut finished = daemon.receive_pairing_finished().await?;
    daemon.request_pair(&device.id).await?;
    println!("Pairing with {}...", device.name);

    let flow = async {
        loop {
            tokio::select! {
                Some(signal) = requested.next() => {
                    let a = signal.args()?;
                    if a.id != device.id {
                        continue;
                    }
                    println!("\n    {}\n", a.code);
                    println!("Check that {} shows the same code.", a.name);
                    let accept = if yes { println!("Accepting (--yes)."); true } else { ask("Do the codes match?").await? };
                    daemon.confirm_pair(&device.id, accept).await?;
                    if accept {
                        println!("Waiting for {} to confirm...", a.name);
                    }
                }
                Some(signal) = finished.next() => {
                    let a = signal.args()?;
                    if a.id != device.id {
                        continue;
                    }
                    if a.success {
                        println!("Paired with {}.", device.name);
                        return Ok(());
                    }
                    bail!("pairing failed: {}", a.message);
                }
            }
        }
    };
    tokio::time::timeout(PAIRING_TIMEOUT, flow)
        .await
        .context("pairing timed out")?
}

async fn listen(daemon: &DaemonProxy<'_>, yes: bool) -> Result<()> {
    let (id, name) = daemon.get_identity().await?;
    println!("Listening as {name} ({id}). Ctrl-C to stop.");
    let mut requested = daemon.receive_pairing_requested().await?;
    let mut finished = daemon.receive_pairing_finished().await?;
    let mut pings = daemon.receive_ping_received().await?;
    let mut transfers = daemon.receive_transfer_changed().await?;
    loop {
        tokio::select! {
            Some(signal) = requested.next() => {
                let a = signal.args()?;
                if !a.incoming {
                    continue;
                }
                println!("\nPairing request from {} ({})", a.name, a.id);
                println!("\n    {}\n", a.code);
                let accept = if yes { println!("Accepting (--yes)."); true } else { ask("Does the other device show the same code?").await? };
                daemon.confirm_pair(a.id, accept).await?;
            }
            Some(signal) = finished.next() => {
                let a = signal.args()?;
                if a.success { println!("Paired with {}.", a.id) } else { println!("Pairing with {} failed: {}", a.id, a.message) }
            }
            Some(signal) = pings.next() => {
                let a = signal.args()?;
                if a.message.is_empty() { println!("Ping from {}", a.name) } else { println!("Ping from {}: {}", a.name, a.message) }
            }
            Some(signal) = transfers.next() => {
                let t = signal.args()?.transfer;
                match (t.incoming, t.state.as_str()) {
                    (true, "waiting") => println!("{} offers {} ({}); accept with the notification or the app", t.device_name, t.name, human_size(t.size)),
                    (true, "done") => println!("Received {} from {}: {}", t.name, t.device_name, t.path),
                    (_, "failed") => println!("Transfer of {} failed: {}", t.name, t.error),
                    _ => {}
                }
            }
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
    }
}
