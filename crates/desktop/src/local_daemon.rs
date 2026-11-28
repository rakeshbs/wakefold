//! Start one independent daemon for the local profile.
use crate::{
    connection::Daemon,
    profiles::{LOCAL, Profile},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::Command,
    time::{Instant, sleep},
};

pub(crate) const PROVIDER_CREDENTIAL: &str = "wakefold-desktop:local-openai";
const START_TIMEOUT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(150);

pub(crate) fn config_dir() -> Option<PathBuf> {
    std::env::var_os("WAKEFOLD_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir().map(|p| p.join("Wakefold")))
}

/// A missing credential is reported to the UI only after the endpoint refuses a connection.
pub(crate) async fn connect(
    runtime: tokio::runtime::Handle,
    directory: Option<&Path>,
    cx: &gpui::AsyncApp,
) -> Result<Option<Daemon>> {
    let client = Daemon::new(&Profile::local(), None, None)?;
    let probe = client.clone();
    if runtime
        .spawn(async move { probe.local_daemon_ready().await })
        .await??
    {
        return Ok(Some(client));
    }
    let directory = directory
        .context("Could not locate the local daemon configuration directory")?
        .to_owned();
    let key = match std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
    {
        Some(key) => key,
        None => {
            let saved = cx
                .update(|app| app.read_credentials(PROVIDER_CREDENTIAL))?
                .await?;
            let Some((_, bytes)) = saved else {
                return Ok(None);
            };
            String::from_utf8(bytes)?
        }
    };
    runtime
        .spawn(async move {
            start(&client, &directory, &key).await?;
            Ok(Some(client))
        })
        .await?
}

fn local_config(directory: &Path) -> Result<PathBuf> {
    let path = directory.join("daemon.json");
    if !path.try_exists()? {
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        let defaults = serde_json::json!({
            "state_directory": "state",
            "defaults": { "model": "gpt-5.4", "max_model_turns": 30, "token_budget": 40_000 }
        });
        serde_json::to_writer_pretty(&mut file, &defaults)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        if let Err(e) = file.persist_noclobber(&path)
            && e.error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(e.into());
        }
    }
    // A user-supplied config must start a daemon for the local HTTP profile.
    #[derive(serde::Deserialize)]
    struct Endpoint {
        bind: Option<SocketAddr>,
        remote: Option<serde::de::IgnoredAny>,
    }
    let endpoint: Endpoint = serde_json::from_reader(File::open(&path)?)
        .with_context(|| format!("Could not read {}", path.display()))?;
    ensure!(
        endpoint.remote.is_none()
            && endpoint
                .bind
                .is_none_or(|bind| format!("http://{bind}/") == LOCAL),
        "{} must use the local listener {LOCAL}",
        path.display()
    );
    Ok(path)
}

fn private_file(path: &Path, append: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .create(true)
        .write(true)
        .append(append)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

/// Hold a cross-process lock from the second health check through startup.
async fn start(client: &Daemon, directory: &Path, key: &str) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    let lock = private_file(&directory.join("daemon-start.lock"), false)?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                if client.local_daemon_ready().await? {
                    return Ok(());
                }
                ensure!(
                    Instant::now() < deadline,
                    "Timed out waiting for another desktop to start the local daemon"
                );
                sleep(POLL).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
    if client.local_daemon_ready().await? {
        return Ok(());
    }
    let config = local_config(directory)?;
    let log_path = directory.join("daemon.log");
    let log = private_file(&log_path, true)?;
    let program = std::env::current_exe()?
        .with_file_name(format!("wakefold{}", std::env::consts::EXE_SUFFIX));
    let mut command = Command::new(&program);
    command
        .arg("serve")
        .arg(&config)
        .env("OPENAI_API_KEY", key)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(false);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    {
        const DETACHED_PROCESS: u32 = 0x8;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = command.spawn().with_context(|| {
        format!(
            "Could not start {}. Install the wakefold daemon beside the desktop executable.",
            program.display()
        )
    })?;
    loop {
        if client.local_daemon_ready().await? {
            // Reap an eventual exit while this app is alive. Closing it leaves the child running.
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "The local daemon exited ({status}). See {}",
                log_path.display()
            );
        }
        ensure!(
            Instant::now() < deadline,
            "The local daemon is still starting. See {}",
            log_path.display()
        );
        sleep(POLL).await;
    }
}
