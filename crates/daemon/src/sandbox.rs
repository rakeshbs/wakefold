//! Reusable process sandboxes with a workspace policy fixed for each agent.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use wakefold::{CancellationToken, RunWorkspace, Tool, ToolContext, ToolError, ToolSpec};

use crate::DaemonError;

const RUNNER_NAME: &str = "runner.py";
const GUEST_RUNNER: &str = "/wakefold-host/runner.py";
const GUEST_WORKSPACE: &str = "/wakefold-chat";
const START_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_FRAME: usize = 8 * 1024 * 1024;
const RUNTIME_PATHS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc/ld.so.cache",
    "/etc/alternatives",
];

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Backend {
    Bubblewrap,
    AppleContainer {
        image: String,
        #[serde(default = "default_container")]
        executable: PathBuf,
        #[serde(default = "default_vm_memory")]
        vm_memory_mib: NonZeroU32,
    },
}

fn default_container() -> PathBuf {
    "container".into()
}

fn default_vm_memory() -> NonZeroU32 {
    NonZeroU32::new(1024).expect("the VM memory limit is positive")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    backend: Backend,
    memory_mib: NonZeroU32,
    #[serde(default = "default_processes")]
    max_processes: NonZeroU32,
    #[serde(default = "default_idle_seconds")]
    idle_seconds: NonZeroU32,
    #[serde(default = "default_timeout")]
    call_timeout_seconds: NonZeroU32,
    #[serde(default)]
    read_peers: BTreeMap<i64, Vec<i64>>,
}

fn default_processes() -> NonZeroU32 {
    NonZeroU32::new(32).expect("the process limit is positive")
}

fn default_idle_seconds() -> NonZeroU32 {
    NonZeroU32::new(30).expect("the idle timeout is positive")
}

fn default_timeout() -> NonZeroU32 {
    NonZeroU32::new(30).expect("the call timeout is positive")
}

#[derive(Serialize)]
struct Mount(PathBuf, PathBuf, bool);

fn agent_folder(id: i64) -> PathBuf {
    PathBuf::from("agents").join(id.to_string())
}

fn mounts(workspace: &RunWorkspace, peers: &[i64], guest: bool) -> Result<Vec<Mount>, DaemonError> {
    let mut mounts = Vec::with_capacity(peers.len() + 1);
    let root = workspace.chat_root();
    if !fs::symlink_metadata(root)?.is_dir() || !fs::symlink_metadata(root.join("agents"))?.is_dir()
    {
        return Err("sandbox workspace must contain a real agents directory".into());
    }
    let agent = workspace.agent_id();
    for (id, writable) in
        std::iter::once((agent, true)).chain(peers.iter().map(|&peer| (peer, false)))
    {
        let folder = agent_folder(id);
        let host_path = root.join(&folder);
        // The workspace's immediate agent directories are managed by Wakefold.
        // A peer from another chat has no directory beneath this root.
        if !fs::symlink_metadata(&host_path)?.is_dir() {
            return Err(format!("agent {id} does not have a real folder in this chat").into());
        }
        let source = if guest {
            Path::new(GUEST_WORKSPACE).join(&folder)
        } else {
            host_path
        };
        mounts.push(Mount(
            source,
            Path::new("/workspace").join(folder),
            writable,
        ));
    }
    Ok(mounts)
}

#[derive(Serialize)]
struct Profile {
    agent: i64,
    mounts: Vec<Mount>,
    guest_cgroup: bool,
    memory_bytes: u64,
    max_processes: u32,
    runtime_paths: &'static [&'static str],
}

struct Chat {
    name: String,
    marker: PathBuf,
    started: AtomicBool,
}

type AgentSession = Arc<Mutex<Option<Session>>>;

pub(crate) struct Sandbox {
    config: Config,
    runner: tempfile::TempDir,
    containers: PathBuf,
    agents: Mutex<BTreeMap<i64, AgentSession>>,
    chats: Mutex<BTreeMap<PathBuf, Arc<Chat>>>,
}

impl Sandbox {
    pub(crate) async fn new(config: Config, state: &Path) -> Result<Self, DaemonError> {
        match &config.backend {
            Backend::Bubblewrap if cfg!(target_os = "linux") => {}
            Backend::AppleContainer {
                image,
                vm_memory_mib,
                ..
            } if cfg!(all(target_os = "macos", target_arch = "aarch64"))
                && !image.trim().is_empty()
                && vm_memory_mib.get() > config.memory_mib.get() => {}
            _ => {
                return Err(
                    "sandbox backend or its configuration is unsupported on this host".into(),
                );
            }
        }
        for (&id, peers) in &config.read_peers {
            if id <= 0 || peers.iter().any(|&peer| peer <= 0 || peer == id) {
                return Err("sandbox peer grants require positive, distinct agent IDs".into());
            }
        }
        let state = state.canonicalize()?;
        if matches!(config.backend, Backend::Bubblewrap) {
            for path in RUNTIME_PATHS {
                match fs::canonicalize(path) {
                    Ok(runtime) if runtime.starts_with(&state) || state.starts_with(&runtime) => {
                        return Err("Wakefold state overlaps a Linux sandbox runtime mount".into());
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        let state = state.join("sandbox");
        let containers = state.join("containers");
        fs::create_dir_all(&containers)?;
        let runner = tempfile::tempdir_in(&state)?;
        fs::write(
            runner.path().join(RUNNER_NAME),
            include_bytes!("sandbox/runner.py"),
        )?;
        let sandbox = Self {
            config,
            runner,
            containers,
            agents: Mutex::default(),
            chats: Mutex::default(),
        };
        if matches!(sandbox.config.backend, Backend::AppleContainer { .. }) {
            sandbox.remove_previous_chats().await?;
        }
        Ok(sandbox)
    }

    async fn run(
        &self,
        workspace: &RunWorkspace,
        argv: &[String],
        input: &Value,
    ) -> Result<Value, DaemonError> {
        let id = workspace.agent_id();
        let slot = self.agents.lock().await.entry(id).or_default().clone();
        let mut slot = slot.lock().await;
        // A cancelled future drops the checked-out process. A subsequent call
        // gets a fresh sandbox instead of inheriting the interrupted protocol.
        let mut previous = slot.take();
        if let Some(session) = &mut previous
            && session.child.try_wait()?.is_some()
        {
            previous = None;
        }
        let mut session = match previous {
            Some(session) => session,
            None => self.start(workspace).await?,
        };
        let duration = Duration::from_secs(self.config.call_timeout_seconds.get().into());
        let result = timeout(
            duration + Duration::from_secs(3),
            session.call(argv, input, duration),
        )
        .await
        .map_err(|_| "sandbox supervisor did not respond before its deadline")??;
        session.used = Instant::now();
        *slot = Some(session);
        result.into_result()
    }

    async fn start(&self, workspace: &RunWorkspace) -> Result<Session, DaemonError> {
        let agent = workspace.agent_id();
        if !(1..(i64::from(u32::MAX) - 10000)).contains(&agent) {
            return Err("agent ID cannot be mapped to a sandbox user".into());
        }
        let peers = self
            .config
            .read_peers
            .get(&agent)
            .map_or(&[][..], Vec::as_slice);
        let guest = matches!(self.config.backend, Backend::AppleContainer { .. });
        let profile = Profile {
            agent,
            mounts: mounts(workspace, peers, guest)?,
            guest_cgroup: guest,
            memory_bytes: u64::from(self.config.memory_mib.get()) * 1024 * 1024,
            max_processes: self.config.max_processes.get(),
            runtime_paths: RUNTIME_PATHS,
        };
        let (mut command, chat) = match &self.config.backend {
            Backend::Bubblewrap => {
                let mut command = Command::new("/usr/bin/systemd-run");
                command.args(["--user", "--scope", "--quiet", "--collect"]);
                command.args([
                    format!("--property=MemoryMax={}", profile.memory_bytes),
                    "--property=MemorySwapMax=0".into(),
                    format!("--property=TasksMax={}", profile.max_processes),
                ]);
                command
                    .args(["--", "/usr/bin/python3", "-I", "-u"])
                    .arg(self.runner.path().join(RUNNER_NAME));
                (command, None)
            }
            Backend::AppleContainer { executable, .. } => {
                let chat = self.apple_chat(workspace.chat_root()).await?;
                let mut command = apple_command(executable);
                command
                    .args(["exec", "--interactive", "--user", "0:0"])
                    .arg(&chat.name)
                    .args(["/usr/bin/python3", "-I", "-u", GUEST_RUNNER]);
                (command, Some(chat))
            }
        };
        command.arg(serde_json::to_string(&profile)?);
        timeout(START_TIMEOUT, Session::start(command, chat))
            .await
            .map_err(|_| "sandbox did not start before its deadline")?
    }

    async fn apple_chat(&self, root: &Path) -> Result<Arc<Chat>, DaemonError> {
        let Backend::AppleContainer {
            executable,
            image,
            vm_memory_mib,
        } = &self.config.backend
        else {
            return Err("Apple container backend is unavailable".into());
        };
        let mut chats = self.chats.lock().await;
        if let Some(chat) = chats.get(root) {
            if chat.started.load(Ordering::Relaxed) {
                return Ok(chat.clone());
            }
            self.remove_chat(chat).await?;
            chats.remove(root);
        }
        let workspace = bind_spec(root, GUEST_WORKSPACE, false)?;
        let runner = bind_spec(self.runner.path(), "/wakefold-host", true)?;
        let marker = tempfile::Builder::new()
            .prefix("wakefold-sandbox-")
            .tempfile_in(&self.containers)?;
        let name = marker
            .path()
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or("invalid container name")?
            .to_owned();
        let (_, marker) = marker.keep()?;
        let chat = Arc::new(Chat {
            name,
            marker,
            started: AtomicBool::new(false),
        });
        // Track the container before awaiting its startup so cancellation
        // still leaves it visible to the idle collector.
        chats.insert(root.to_owned(), chat.clone());
        let mut command = apple_command(executable);
        command
            .args([
                "run",
                "--detach",
                "--rm",
                "--network",
                "none",
                "--read-only",
                // Bubblewrap must be able to disable nested user namespaces
                // in the guest. The agent mount namespace has no host procfs.
                "--read-only-path",
                "NONE",
                "--cap-add",
                "SYS_ADMIN",
                "--memory",
            ])
            .arg(format!("{}M", vm_memory_mib.get()))
            .arg("--name")
            .arg(&chat.name)
            .arg("--mount")
            .arg(workspace)
            .arg("--mount")
            .arg(runner)
            .args(["--entrypoint", "/usr/bin/python3"])
            .arg(image)
            .args(["-I", "-u", GUEST_RUNNER, "--idle-chat"]);
        if let Err(error) = apple_output(command).await {
            if self.remove_chat(&chat).await.is_ok() {
                chats.remove(root);
            }
            return Err(error);
        }
        chat.started.store(true, Ordering::Relaxed);
        Ok(chat)
    }

    async fn remove_previous_chats(&self) -> Result<(), DaemonError> {
        for entry in fs::read_dir(&self.containers)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "invalid saved sandbox name")?;
            if entry.file_type()?.is_file() && name.starts_with("wakefold-sandbox-") {
                self.remove_chat(&Chat {
                    name,
                    marker: entry.path(),
                    started: AtomicBool::new(false),
                })
                .await?;
            }
        }
        Ok(())
    }

    async fn remove_chat(&self, chat: &Chat) -> Result<(), DaemonError> {
        let Backend::AppleContainer { executable, .. } = &self.config.backend else {
            return Ok(());
        };
        let mut command = apple_command(executable);
        command.args(["rm", "--force", &chat.name]);
        if let Err(remove) = apple_output(command).await {
            // Auto-remove may already have removed the container. Require a
            // successful inventory read before treating that as cleanup.
            let mut command = apple_command(executable);
            command.args(["list", "--all", "--format", "json"]);
            let output = apple_output(command).await?;
            if String::from_utf8_lossy(&output).contains(&chat.name) {
                return Err(remove);
            }
        }
        fs::remove_file(&chat.marker)?;
        Ok(())
    }

    pub(crate) async fn maintain(&self, stop: &CancellationToken) {
        let idle = Duration::from_secs(self.config.idle_seconds.get().into());
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = interval.tick() => {}
            }
            self.agents.lock().await.retain(|_, slot| {
                Arc::strong_count(slot) > 1
                    || slot.try_lock().ok().is_some_and(|slot| {
                        slot.as_ref()
                            .is_some_and(|session| session.used.elapsed() < idle)
                    })
            });
            self.reap_chats(false).await;
        }
        self.agents.lock().await.clear();
        self.reap_chats(true).await;
    }

    async fn reap_chats(&self, shutdown: bool) {
        let mut chats = self.chats.lock().await;
        let paths: Vec<_> = chats
            .iter()
            .filter(|(_, chat)| shutdown || Arc::strong_count(chat) == 1)
            .map(|(path, _)| path.clone())
            .collect();
        for path in paths {
            match self.remove_chat(&chats[&path]).await {
                Ok(()) => {
                    chats.remove(&path);
                }
                Err(error) => tracing::warn!(%error, "could not remove sandbox chat"),
            }
        }
    }
}

fn bind_spec(path: &Path, guest: &str, readonly: bool) -> Result<String, DaemonError> {
    let path = path
        .to_str()
        .filter(|value| !value.contains(','))
        .ok_or("sandbox mount paths must be UTF-8 and cannot contain commas")?;
    let protection = if readonly { ",readonly" } else { "" };
    Ok(format!(
        "type=bind,source={path},target={guest}{protection}"
    ))
}

fn apple_command(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command.kill_on_drop(true);
    command
}

async fn apple_output(mut command: Command) -> Result<Vec<u8>, DaemonError> {
    let output = timeout(START_TIMEOUT, command.output()).await??;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "Apple container failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into())
    }
}

struct Session {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    used: Instant,
    _chat: Option<Arc<Chat>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Reply {
    Output { value: Value },
    Failure { error: String },
}

impl Reply {
    fn into_result(self) -> Result<Value, DaemonError> {
        match self {
            Self::Output { value } => Ok(value),
            Self::Failure { error } => Err(error.into()),
        }
    }
}

impl Session {
    async fn start(mut command: Command, chat: Option<Arc<Chat>>) -> Result<Self, DaemonError> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().ok_or("missing sandbox stdin")?;
        let output = child.stdout.take().ok_or("missing sandbox stdout")?;
        let mut session = Self {
            child,
            input,
            output: BufReader::new(output),
            used: Instant::now(),
            _chat: chat,
        };
        let ready: Value = session.read().await?;
        if ready != json!({"ready": true}) {
            return Err("sandbox failed to acknowledge isolation".into());
        }
        Ok(session)
    }

    async fn read<T: serde::de::DeserializeOwned>(&mut self) -> Result<T, DaemonError> {
        let mut line = Vec::new();
        (&mut self.output)
            .take(MAX_FRAME as u64)
            .read_until(b'\n', &mut line)
            .await?;
        if line.last() != Some(&b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sandbox closed or exceeded the response limit",
            )
            .into());
        }
        Ok(serde_json::from_slice(&line)?)
    }

    async fn call(
        &mut self,
        argv: &[String],
        data: &Value,
        duration: Duration,
    ) -> Result<Reply, DaemonError> {
        let mut frame = serde_json::to_vec(&json!({
            "argv": argv, "input": data, "timeout_ms": duration.as_millis(),
        }))?;
        if frame.len() >= MAX_FRAME {
            return Err("sandbox command input is too large".into());
        }
        frame.push(b'\n');
        self.input.write_all(&frame).await?;
        self.read().await
    }
}

pub(crate) struct SandboxedCommand {
    spec: ToolSpec,
    argv: Vec<String>,
    sandbox: Arc<Sandbox>,
}

impl SandboxedCommand {
    pub(crate) fn new(
        name: String,
        description: String,
        command: String,
        args: Vec<String>,
        parameters: Value,
        sandbox: Arc<Sandbox>,
    ) -> Result<Self, DaemonError> {
        if !command.starts_with('/')
            || command.contains('\0')
            || args.iter().any(|arg| arg.contains('\0'))
        {
            return Err("sandbox tool command must be an absolute guest executable path with valid arguments".into());
        }
        if !parameters.is_object() {
            return Err("sandbox tool parameters must be a JSON Schema object".into());
        }
        let argv = std::iter::once(command).chain(args).collect();
        Ok(Self {
            spec: ToolSpec::new(name, description, parameters),
            argv,
            sandbox,
        })
    }
}

#[async_trait]
impl Tool for SandboxedCommand {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn call(&self, _: &Value) -> Result<Value, ToolError> {
        Err(ToolError::new("sandbox commands require a run workspace"))
    }

    async fn call_with_context(
        &self,
        context: ToolContext,
        arguments: &Value,
    ) -> Result<Value, ToolError> {
        let workspace = context
            .workspace()
            .ok_or_else(|| ToolError::new("sandbox commands require a run workspace"))?;
        self.sandbox
            .run(workspace, &self.argv, arguments)
            .await
            .map_err(|error| ToolError::new(error.to_string()))
    }
}

#[cfg(test)]
mod tests;
