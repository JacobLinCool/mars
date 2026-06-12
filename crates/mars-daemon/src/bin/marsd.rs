#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use mars_daemon::{MarsDaemon, setup_logging};
use mars_types::DEFAULT_LOG_PATH_RELATIVE;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

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

// The async runtime only serves IPC requests and housekeeping; every audio
// path runs on dedicated OS threads outside tokio (marsd-render, sink and
// capture workers). The default worker count (one per core — 18 on larger
// Apple Silicon machines) just burns thread stacks and per-thread malloc
// magazines; two workers are ample.
#[tokio::main(worker_threads = 2)]
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
    let daemon_task = tokio::spawn({
        let daemon = daemon.clone();
        let socket_path = socket_path.clone();
        async move { daemon.run(&socket_path).await }
    });

    #[cfg(unix)]
    {
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            result = daemon_task => {
                result??;
            }
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        tokio::select! {
            result = daemon_task => {
                result??;
            }
            _ = tokio::signal::ctrl_c() => {}
        }
    }

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
