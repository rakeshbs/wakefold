//! The local Wakefold daemon.

mod api;
mod config;
mod mcp;
mod remote;
mod replay;
#[cfg(feature = "sandbox")]
mod sandbox;
mod server;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;

use tokio::net::TcpListener;
use wakefold::{CancellationToken, OpenAiProvider};

use crate::config::{Config, Runtime};

type DaemonError = Box<dyn Error + Send + Sync>;
const USAGE: &str =
    "Usage: wakefold serve <config.json>\n       wakefold replay <state-directory> <agent-id>";

fn init_logging() -> Result<(), DaemonError> {
    tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .try_init()
}

#[tokio::main]
async fn main() -> Result<(), DaemonError> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => println!("{USAGE}"),
        [flag] if flag == "--help" || flag == "-h" => {
            println!("{USAGE}")
        }
        [flag] if flag == "--version" || flag == "-V" => {
            println!("Wakefold {}", env!("CARGO_PKG_VERSION"));
        }
        [command, state, id] if command == "replay" => {
            let id = id
                .to_str()
                .and_then(|id| id.parse::<i64>().ok())
                .filter(|&id| id > 0)
                .ok_or("agent-id must be a positive integer")?;
            replay::write(Path::new(state), id, std::io::stdout().lock())?;
        }
        [command, path] if command == "serve" => {
            init_logging()?;
            let config = Config::load(Path::new(path))?;
            let key = std::env::var("OPENAI_API_KEY")
                .map_err(|_| "set OPENAI_API_KEY before starting the daemon")?;
            let runtime = Runtime::build(config, BTreeMap::new(), |model| {
                Ok(Box::new(OpenAiProvider::new(model, &key)?))
            })
            .await?;
            let listener = TcpListener::bind(runtime.bind).await?;
            let scheme = if runtime.remote.is_some() {
                "https"
            } else {
                "http"
            };
            println!(
                "Wakefold listening on {scheme}://{}",
                listener.local_addr()?
            );
            let stop = CancellationToken::new();
            let signal_stop = stop.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    signal_stop.cancel();
                }
            });
            server::serve(listener, runtime, stop).await?;
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}
