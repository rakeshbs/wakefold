//! The local Wakefold daemon.

mod api;
mod config;
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

#[tokio::main]
async fn main() -> Result<(), DaemonError> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => println!("Usage: wakefold serve <config.json>"),
        [flag] if flag == "--help" || flag == "-h" => {
            println!("Usage: wakefold serve <config.json>")
        }
        [flag] if flag == "--version" || flag == "-V" => {
            println!("Wakefold {}", env!("CARGO_PKG_VERSION"));
        }
        [command, path] if command == "serve" => {
            let config = Config::load(Path::new(path))?;
            let key = std::env::var("OPENAI_API_KEY")
                .map_err(|_| "set OPENAI_API_KEY before starting the daemon")?;
            let runtime = Runtime::build(config, BTreeMap::new(), |model| {
                Ok(Box::new(OpenAiProvider::new(model, &key)?))
            })?;
            let listener = TcpListener::bind(runtime.bind).await?;
            println!("Wakefold listening on http://{}", listener.local_addr()?);
            let stop = CancellationToken::new();
            let signal_stop = stop.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    signal_stop.cancel();
                }
            });
            server::serve(listener, runtime, stop).await?;
        }
        _ => return Err("usage: wakefold serve <config.json>".into()),
    }
    Ok(())
}
