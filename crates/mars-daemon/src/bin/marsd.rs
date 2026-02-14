#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use mars_daemon::{MarsDaemon, setup_logging};
use mars_types::DEFAULT_LOG_PATH_RELATIVE;

#[derive(Debug, Parser)]
#[command(name = "marsd")]
#[command(about = "MARS audio routing daemon")]
struct Args {
    /// Start IPC server (default behavior)
    #[arg(long, default_value_t = true)]
    serve: bool,
    /// Override socket path
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Print the resolved socket path and exit
    #[arg(long)]
    print_socket: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let socket_path = args.socket.unwrap_or_else(default_socket_path);

    if args.print_socket {
        println!("{}", socket_path.display());
        return Ok(());
    }

    if !args.serve {
        return Ok(());
    }

    let _guard = setup_logging()?;
    let daemon = Arc::new(MarsDaemon::new(default_log_path()));
    daemon.run(&socket_path).await?;
    Ok(())
}

fn default_socket_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(mars_types::DEFAULT_SOCKET_PATH_RELATIVE)
}

fn default_log_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(DEFAULT_LOG_PATH_RELATIVE)
}
