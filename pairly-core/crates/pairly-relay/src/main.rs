//! `pairly-relay`: run the blind relay. Every flag can also be set through the environment
//! (`PAIRLY_RELAY_*`), which is how the Docker image is configured.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use pairly_relay::{Config, Relay};
use pairly_transport_relay::proto::{DEFAULT_PORT, RelayAddr};
use pairly_transport_relay::tls;
use tokio::signal::unix::{SignalKind, signal};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    version,
    about = "Pairly blind relay: lets paired devices reach each other over the internet"
)]
struct Args {
    /// UDP address to listen on.
    #[arg(long, env = "PAIRLY_RELAY_LISTEN", default_value_t = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], DEFAULT_PORT)))]
    listen: SocketAddr,
    /// Where the relay keeps its certificate (created on first start).
    #[arg(
        long,
        env = "PAIRLY_RELAY_DATA",
        default_value = "/var/lib/pairly-relay"
    )]
    data_dir: PathBuf,
    /// Access tokens (comma separated). Devices must present one; without any, anyone who
    /// knows the address can use the relay.
    #[arg(long = "token", env = "PAIRLY_RELAY_TOKENS", value_delimiter = ',')]
    tokens: Vec<String>,
    /// This relay's public host name or IP, used to print the address to paste into Pairly.
    #[arg(long, env = "PAIRLY_RELAY_PUBLIC_HOST")]
    public_host: Option<String>,
    /// UDP port devices reach the relay on, if different from the listen port (NAT, Docker).
    #[arg(long, env = "PAIRLY_RELAY_PUBLIC_PORT")]
    public_port: Option<u16>,
    #[arg(long, env = "PAIRLY_RELAY_MAX_CONNS_PER_IP", default_value_t = 32)]
    max_conns_per_ip: usize,
    #[arg(long, env = "PAIRLY_RELAY_MAX_ROOMS_PER_CONN", default_value_t = 64)]
    max_rooms_per_conn: usize,
    /// Bandwidth cap per connection pair and direction, in megabits per second (0: none).
    #[arg(long, env = "PAIRLY_RELAY_RATE_LIMIT_MBPS", default_value_t = 0)]
    rate_limit_mbps: u64,
    /// Seconds between statistics lines in the log.
    #[arg(long, env = "PAIRLY_RELAY_STATS_INTERVAL", default_value_t = 300)]
    stats_interval: u64,
}

/// Load the relay's certificate, or create one. Its hash is the pin in every device's relay
/// address, so it must survive restarts.
fn load_or_create_cert(dir: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    let (cert_path, key_path) = (dir.join("cert.der"), dir.join("key.der"));
    if cert_path.exists() && key_path.exists() {
        return Ok((std::fs::read(&cert_path)?, std::fs::read(&key_path)?));
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let (cert, key) = tls::generate_cert().map_err(anyhow::Error::from_boxed)?;
    std::fs::write(&cert_path, &cert)?;
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_path)
            .with_context(|| format!("creating {}", key_path.display()))?;
        f.write_all(&key)?;
    }
    info!(dir = %dir.display(), "created a new relay certificate");
    Ok((cert, key))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("PAIRLY_RELAY_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let (cert, key) = load_or_create_cert(&args.data_dir)?;
    let config = Config {
        listen: args.listen,
        tokens: args
            .tokens
            .iter()
            .filter(|t| !t.is_empty())
            .cloned()
            .collect(),
        max_conns_per_ip: args.max_conns_per_ip,
        max_rooms_per_conn: args.max_rooms_per_conn,
        rate_limit: (args.rate_limit_mbps > 0).then(|| args.rate_limit_mbps * 1_000_000 / 8),
    };
    let relay = Relay::start(config.clone(), cert, key)
        .with_context(|| format!("listening on {}", args.listen))?;
    let port = args.public_port.unwrap_or(relay.local_addr()?.port());
    let host = args
        .public_host
        .clone()
        .unwrap_or_else(|| "YOUR-SERVER-IP".to_owned());
    let address = RelayAddr {
        host,
        port,
        pin: relay.pin(),
        token: config.tokens.first().cloned(),
    };
    info!(listen = %relay.local_addr()?, tokens = config.tokens.len(), "pairly-relay {} ready", env!("CARGO_PKG_VERSION"));
    // Printed plainly (not as a log field) so it is easy to copy.
    println!("Relay address (put it in the PC's config.toml under [relay]):\n  {address}");
    if config.tokens.is_empty() {
        println!("Warning: no access token set; anyone with this address can use the relay.");
    }

    let stats = relay.stats_handle();
    let report = tokio::spawn(pairly_relay::report_stats(
        Arc::clone(&stats),
        Duration::from_secs(args.stats_interval.max(10)),
    ));
    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    info!("shutting down: {}", stats.summary());
    report.abort();
    relay.shutdown().await;
    Ok(())
}
