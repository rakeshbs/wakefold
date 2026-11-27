//! On-disk daemon configuration and the host tool catalog.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::fs::{self, File};
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use wakefold::{
    DelegateTool, Provider, PublishArtifact, RunOptions, SendMessage, TimerWait, Tool, ToolGrant,
    WaitForMessage,
};

use crate::DaemonError;
use crate::mcp::{McpServer, ServerConfig};
#[cfg(feature = "sandbox")]
use crate::sandbox::{self, Sandbox, SandboxedCommand};
#[cfg(feature = "sandbox")]
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(skip)]
    directory: PathBuf,
    pub(crate) state_directory: PathBuf,
    #[serde(default = "default_bind")]
    pub(crate) bind: SocketAddr,
    #[serde(default = "default_concurrency")]
    max_concurrent: NonZeroUsize,
    #[serde(default)]
    skills: BTreeMap<String, SkillConfig>,
    #[serde(default)]
    mcp_servers: BTreeMap<String, ServerConfig>,
    #[cfg(feature = "sandbox")]
    #[serde(default)]
    sandbox: Option<sandbox::Config>,
    #[serde(default)]
    agents: BTreeMap<String, AgentConfig>,
    #[serde(default)]
    defaults: Option<wakefold::AgentSettings>,
}

impl Config {
    pub(crate) fn load(path: &Path) -> Result<Self, DaemonError> {
        let path = path.canonicalize()?;
        let mut config: Self = serde_json::from_slice(&std::fs::read(&path)?)?;
        config.directory = path
            .parent()
            .expect("the canonical configuration file has a parent")
            .to_owned();
        if config.state_directory.is_relative() {
            config.state_directory = config.directory.join(config.state_directory);
        }
        Ok(config)
    }
}

fn default_bind() -> SocketAddr {
    (Ipv4Addr::LOCALHOST, 8700).into()
}

fn default_concurrency() -> NonZeroUsize {
    NonZeroUsize::new(4).expect("default concurrency is nonzero")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentConfig {
    model: String,
    max_model_turns: NonZeroUsize,
    token_budget: u64,
    #[serde(default)]
    context_byte_budget: Option<NonZeroUsize>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    tool_grants: BTreeMap<String, Grant>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Grant {
    Allow,
    RequireApproval,
}

impl From<Grant> for ToolGrant {
    fn from(grant: Grant) -> Self {
        match grant {
            Grant::Allow => Self::Allow,
            Grant::RequireApproval => Self::RequireApproval,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillConfig {
    description: String,
    tools: Vec<ToolConfig>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ToolConfig {
    TimerWait,
    SendMessage,
    WaitForMessage,
    Delegate {
        name: String,
        description: String,
        agent: String,
    },
    Mcp {
        server: String,
        tool: String,
        name: Option<String>,
    },
    #[cfg(feature = "sandbox")]
    SandboxCommand {
        name: String,
        description: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        parameters: serde_json::Value,
    },
}

pub(crate) struct RegisteredSkill {
    pub(crate) description: String,
    pub(crate) tools: Vec<Box<dyn Tool>>,
}

pub(crate) struct Agent {
    pub(crate) provider: Box<dyn Provider>,
    pub(crate) skills: Vec<String>,
    pub(crate) options: RunOptions,
}

pub(crate) struct SharedProfile {
    pub(crate) provider: Box<dyn Provider>,
    pub(crate) options: RunOptions,
}

pub(crate) struct Runtime {
    pub(crate) state_directory: PathBuf,
    pub(crate) bind: SocketAddr,
    pub(crate) max_concurrent: NonZeroUsize,
    pub(crate) skills: BTreeMap<String, RegisteredSkill>,
    pub(crate) agents: BTreeMap<String, Agent>,
    pub(crate) defaults: Option<wakefold::AgentSettings>,
    pub(crate) profiles: BTreeMap<wakefold::AgentSettings, SharedProfile>,
    pub(crate) mcp_servers: BTreeMap<String, McpServer>,
    #[cfg(feature = "sandbox")]
    pub(crate) sandbox: Option<Arc<Sandbox>>,
    _state_lock: File,
}

impl Runtime {
    pub(crate) async fn build(
        config: Config,
        mut skills: BTreeMap<String, RegisteredSkill>,
        mut provider: impl FnMut(&str) -> Result<Box<dyn Provider>, DaemonError>,
    ) -> Result<Self, DaemonError> {
        if !config.bind.ip().is_loopback() {
            return Err("daemon bind address must be loopback".into());
        }
        if config.agents.is_empty() && config.defaults.is_none() {
            return Err("configure at least one agent".into());
        }
        // Own the state directory before starting the configured server processes.
        fs::create_dir_all(&config.state_directory)?;
        let state_lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(config.state_directory.join("daemon.lock"))?;
        state_lock
            .try_lock()
            .map_err(|error| format!("could not lock Wakefold state directory: {error}"))?;
        #[cfg(feature = "sandbox")]
        let sandbox = match config.sandbox {
            Some(settings) => Some(Arc::new(
                Sandbox::new(settings, &config.state_directory).await?,
            )),
            None => None,
        };
        let mut mcp_servers = BTreeMap::new();
        for (name, skill) in config.skills {
            let mut tools: Vec<Box<dyn Tool>> = Vec::new();
            for tool in skill.tools {
                if config.defaults.is_some() && matches!(tool, ToolConfig::Delegate { .. }) {
                    return Err("shared defaults use agents.spawn for delegation".into());
                }
                tools.push(match tool {
                    ToolConfig::TimerWait => Box::new(TimerWait),
                    ToolConfig::SendMessage => Box::new(SendMessage),
                    ToolConfig::WaitForMessage => Box::new(WaitForMessage),
                    ToolConfig::Delegate {
                        name,
                        description,
                        agent,
                    } => {
                        if !config.agents.contains_key(&agent) {
                            return Err(format!(
                                "delegate tool {name} names an unknown agent {agent}"
                            )
                            .into());
                        }
                        Box::new(DelegateTool::new(name, description, agent))
                    }
                    ToolConfig::Mcp {
                        server,
                        tool,
                        name: local_name,
                    } => {
                        let connection = match mcp_servers.entry(server.clone()) {
                            Entry::Occupied(entry) => entry.into_mut(),
                            Entry::Vacant(entry) => {
                                let definition =
                                    config.mcp_servers.get(&server).ok_or_else(|| {
                                        format!(
                                            "skill {name} references unknown MCP server {server}"
                                        )
                                    })?;
                                let connection =
                                    McpServer::start(&server, definition, &config.directory)
                                        .await?;
                                entry.insert(connection)
                            }
                        };
                        connection.tool(&server, tool, local_name)?
                    }
                    #[cfg(feature = "sandbox")]
                    ToolConfig::SandboxCommand {
                        name,
                        description,
                        command,
                        args,
                        parameters,
                    } => {
                        let sandbox = sandbox
                            .clone()
                            .ok_or("sandbox_command requires sandbox settings")?;
                        Box::new(SandboxedCommand::new(
                            name,
                            description,
                            command,
                            args,
                            parameters,
                            sandbox,
                        )?)
                    }
                });
            }
            if skills
                .insert(
                    name.clone(),
                    RegisteredSkill {
                        description: skill.description,
                        tools,
                    },
                )
                .is_some()
            {
                return Err(format!("duplicate skill {name}").into());
            }
        }
        skills
            .entry("artifacts".into())
            .or_insert_with(|| RegisteredSkill {
                description: "Publish finished files from the chat workspace".into(),
                tools: vec![Box::new(PublishArtifact)],
            });
        let mut defaults = config.defaults;
        let saved =
            wakefold::WorkspaceStore::open(&config.state_directory)?.active_agent_settings()?;
        if defaults.is_some() || !saved.is_empty() {
            for (name, description, tools) in [
                (
                    "agents",
                    "Spawn independent subagents",
                    vec![Box::new(wakefold::SpawnAgent) as Box<dyn Tool>],
                ),
                (
                    "mailbox",
                    "Send messages, check replies, or wait for child outcomes",
                    vec![
                        Box::new(SendMessage) as Box<dyn Tool>,
                        Box::new(WaitForMessage),
                    ],
                ),
            ] {
                if skills
                    .insert(
                        name.into(),
                        RegisteredSkill {
                            description: description.into(),
                            tools,
                        },
                    )
                    .is_some()
                {
                    return Err(format!("{name} is a built-in dynamic agent skill").into());
                }
            }
        }
        if let Some(settings) = &mut defaults {
            settings.catalog = skills
                .iter()
                .map(|(name, s)| (name.clone(), s.description.clone()))
                .collect();
            let mut grants = BTreeMap::new();
            for (name, skill) in &skills {
                for tool in &skill.tools {
                    let qualified = format!("{name}.{}", tool.spec().name);
                    let grant = settings
                        .tool_grants
                        .remove(&qualified)
                        .unwrap_or(ToolGrant::Allow);
                    if grants.insert(qualified.clone(), grant).is_some() {
                        return Err(format!("duplicate tool {qualified}").into());
                    }
                }
            }
            if let Some(unknown) = settings.tool_grants.keys().next() {
                return Err(format!("unknown tool {unknown}").into());
            }
            settings.tool_grants = grants;
        }
        let mut profiles = BTreeMap::new();
        for settings in saved.into_iter().chain(defaults.iter().cloned()) {
            if let Entry::Vacant(entry) = profiles.entry(settings) {
                let settings = entry.key();
                let mut options = RunOptions::new(settings.max_model_turns.get())
                    .with_token_budget(settings.token_budget)
                    .with_tool_grants(settings.tool_grants.iter().filter_map(|(name, saved)| {
                        let current = match &defaults {
                            Some(defaults) => defaults.tool_grants.get(name)?,
                            None => saved,
                        };
                        let grant = if *current == ToolGrant::RequireApproval {
                            *current
                        } else {
                            *saved
                        };
                        Some((name.as_str(), grant))
                    }));
                if let Some(limit) = settings.context_byte_budget {
                    options = options.with_context_byte_budget(limit.get());
                }
                let provider = provider(&settings.model)?;
                entry.insert(SharedProfile { provider, options });
            }
        }
        let mut agents = BTreeMap::new();
        for (name, agent) in config.agents {
            let mut available_tools = BTreeSet::new();
            let mut skill_names = BTreeSet::new();
            for skill in &agent.skills {
                if !skill_names.insert(skill) {
                    return Err(format!("agent {name} lists skill {skill} twice").into());
                }
                let registered = skills
                    .get(skill)
                    .ok_or_else(|| format!("agent {name} lists an unknown skill {skill}"))?;
                for tool in &registered.tools {
                    let qualified = format!("{skill}.{}", tool.spec().name);
                    if !available_tools.insert(qualified.clone()) {
                        return Err(format!("agent {name} has duplicate tool {qualified}").into());
                    }
                }
            }
            for tool in agent.tool_grants.keys() {
                if !available_tools.contains(tool) {
                    return Err(format!("agent {name} grants an unavailable tool {tool}").into());
                }
            }
            let mut options = RunOptions::new(agent.max_model_turns.get())
                .with_token_budget(agent.token_budget)
                .with_tool_grants(
                    agent
                        .tool_grants
                        .iter()
                        .map(|(name, &grant)| (name.as_str(), grant.into())),
                );
            if let Some(bytes) = agent.context_byte_budget {
                options = options.with_context_byte_budget(bytes.get());
            }
            agents.insert(
                name,
                Agent {
                    provider: provider(&agent.model)?,
                    skills: agent.skills,
                    options,
                },
            );
        }
        Ok(Self {
            state_directory: config.state_directory,
            bind: config.bind,
            max_concurrent: config.max_concurrent,
            skills,
            agents,
            defaults,
            profiles,
            mcp_servers,
            #[cfg(feature = "sandbox")]
            sandbox,
            _state_lock: state_lock,
        })
    }
}
