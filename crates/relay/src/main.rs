//! `open-desk-relay`: run a relay server.

use std::net::{Ipv6Addr, SocketAddr};
use std::path::PathBuf;

use clap::Parser;
use open_desk_relay::{DEFAULT_RELAY_PORT, RelayConfig, RelayServer};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(version, about = "Relay server for open-desk devices behind NATs")]
struct Arguments {
    /// Address for the control endpoint; forwarded ports are opened on the same IP.
    #[arg(long, default_value_t = SocketAddr::from((Ipv6Addr::UNSPECIFIED, DEFAULT_RELAY_PORT)))]
    listen: SocketAddr,
    /// Directory for the relay certificate and the device ID table.
    #[arg(long, default_value = "relay-data")]
    data_dir: PathBuf,
    /// Most connections forwarded at once.
    #[arg(long, default_value_t = 256)]
    max_allocations: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let arguments = Arguments::parse();
    let _relay = RelayServer::start(&RelayConfig {
        listen: arguments.listen,
        data_directory: arguments.data_dir,
        max_allocations: arguments.max_allocations,
    })?;
    tokio::signal::ctrl_c().await?;
    Ok(())
}
