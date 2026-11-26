//! On-disk daemon configuration and the host tool catalog.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use wakefold::{
    DelegateTool, Provider, RunOptions, SendMessage, TimerWait, Tool, ToolGrant, WaitForMessage,
};

use crate::DaemonError;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) state_directory: PathBuf,
    #[serde(default = "default_bind")]
    pub(crate) bind: SocketAddr,
    #[serde(default = "default_concurrency")]
    max_concurrent: NonZeroUsize,
    #[serde(default)]
    skills: BTreeMap<String, SkillConfig>,
    agents: BTreeMap<String, AgentConfig>,
}

impl Config {
    pub(crate) fn load(path: &Path) -> Result<Self, DaemonError> {
        let path = path.canonicalize()?;
        let mut config: Self = serde_json::from_slice(&std::fs::read(&path)?)?;
        if config.state_directory.is_relative() {
            config.state_directory = path
                .parent()
                .expect("the canonical configuration file has a parent")
                .join(config.state_directory);
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

pub(crate) struct Runtime {
    pub(crate) state_directory: PathBuf,
    pub(crate) bind: SocketAddr,
    pub(crate) max_concurrent: NonZeroUsize,
    pub(crate) skills: BTreeMap<String, RegisteredSkill>,
    pub(crate) agents: BTreeMap<String, Agent>,
}

impl Runtime {
    pub(crate) fn build(
        config: Config,
        mut skills: BTreeMap<String, RegisteredSkill>,
        mut provider: impl FnMut(&str) -> Result<Box<dyn Provider>, DaemonError>,
    ) -> Result<Self, DaemonError> {
        if !config.bind.ip().is_loopback() {
            return Err("daemon bind address must be loopback".into());
        }
        if config.agents.is_empty() {
            return Err("configure at least one agent".into());
        }
        for (name, skill) in config.skills {
            let mut tools: Vec<Box<dyn Tool>> = Vec::new();
            for tool in skill.tools {
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
        })
    }
}
