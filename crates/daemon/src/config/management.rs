//! Operator edits to the same configuration file used at daemon startup.
use super::{Config, SkillConfig, ToolConfig};
use crate::{
    DaemonError,
    mcp::{McpServer, ServerConfig},
};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use wakefold::ToolGrant;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Binding {
    skill: String,
    tool: String,
    name: String,
    grant: ToolGrant,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Definition {
    launch: ServerConfig,
    bindings: Vec<Binding>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServerChange {
    expected: Option<Definition>,
    replacement: Option<Definition>,
}

#[derive(Debug)]
pub(crate) struct ManagementError(StatusCode, String);

impl IntoResponse for ManagementError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}

fn invalid(message: impl std::fmt::Display) -> ManagementError {
    ManagementError(StatusCode::UNPROCESSABLE_ENTITY, message.to_string())
}
fn internal(error: impl std::fmt::Display) -> ManagementError {
    tracing::error!(%error, "MCP configuration operation failed");
    ManagementError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not read or save daemon configuration".into(),
    )
}

pub(crate) struct McpManagement {
    path: PathBuf,
    editable: bool,
    active: BTreeMap<String, Definition>,
    tools: BTreeMap<String, Vec<Value>>,
    reserved_skills: BTreeSet<String>,
    editing: tokio::sync::Mutex<()>,
}

impl Config {
    fn definitions(&self) -> BTreeMap<String, Definition> {
        let mut servers: BTreeMap<_, _> = self
            .mcp_servers
            .iter()
            .map(|(name, launch)| {
                (
                    name.clone(),
                    Definition {
                        launch: launch.clone(),
                        bindings: vec![],
                    },
                )
            })
            .collect();
        for (skill, config) in &self.skills {
            for tool in &config.tools {
                if let ToolConfig::Mcp { server, tool, name } = tool
                    && let Some(def) = servers.get_mut(server)
                {
                    let name = name.as_ref().unwrap_or(tool);
                    let grant = self
                        .defaults
                        .tool_grants
                        .get(&format!("{skill}.{name}"))
                        .copied()
                        .unwrap_or(ToolGrant::Allow);
                    def.bindings.push(Binding {
                        skill: skill.clone(),
                        tool: tool.clone(),
                        name: name.clone(),
                        grant,
                    });
                }
            }
        }
        servers
    }
}

impl McpManagement {
    pub(super) fn new(config: &Config, mut reserved_skills: BTreeSet<String>) -> Self {
        reserved_skills.extend(["agents", "mailbox"].map(str::to_owned));
        if !config.skills.contains_key("artifacts") {
            reserved_skills.insert("artifacts".into());
        }
        Self {
            path: config.path.clone(),
            editable: config.allow_mcp_configuration,
            active: config.definitions(),
            tools: BTreeMap::new(),
            reserved_skills,
            editing: Default::default(),
        }
    }
    pub(super) fn capture_tools(&mut self, servers: &BTreeMap<String, McpServer>) {
        self.tools = servers
            .iter()
            .map(|(n, s)| (n.clone(), s.discovered()))
            .collect();
    }
    pub(crate) async fn inventory(&self) -> Result<Value, DaemonError> {
        let saved = if self.editable {
            let bytes = tokio::fs::read(&self.path).await?;
            serde_json::from_slice::<Config>(&bytes)?.definitions()
        } else {
            self.active.clone()
        };
        let names: BTreeSet<_> = self.active.keys().chain(saved.keys()).collect();
        let servers: Vec<_> = names
            .into_iter()
            .map(|name| {
                let current = self.active.get(name);
                let next = saved.get(name);
                json!({"name":name,"restart_required":current!=next,
                "configuration":next.filter(|_|self.editable),
                "bindings":current.map(|d| &d.bindings).into_iter().flatten().collect::<Vec<_>>(),
                "tools":self.tools.get(name).map(Vec::as_slice).unwrap_or_default()})
            })
            .collect();
        Ok(json!({"editable":self.editable,"servers":servers}))
    }

    fn check_permission(&self) -> Result<(), ManagementError> {
        if self.editable {
            Ok(())
        } else {
            Err(ManagementError(
                StatusCode::FORBIDDEN,
                "This daemon has MCP configuration disabled".into(),
            ))
        }
    }
    async fn discover_tools(&self, launch: &ServerConfig) -> Result<Vec<Value>, ManagementError> {
        let directory = self
            .path
            .parent()
            .ok_or_else(|| internal("missing config directory"))?;
        let mut server = McpServer::start("discovery", launch, directory)
            .await
            .map_err(invalid)?;
        let tools = server.discovered();
        server.close("discovery").await;
        Ok(tools)
    }
    pub(crate) async fn discover(
        &self,
        launch: &ServerConfig,
    ) -> Result<Vec<Value>, ManagementError> {
        self.check_permission()?;
        let _lock = self.editing.try_lock().map_err(|_| {
            ManagementError(
                StatusCode::CONFLICT,
                "Another MCP configuration operation is in progress".into(),
            )
        })?;
        self.discover_tools(launch).await
    }
    pub(crate) async fn save(
        &self,
        name: String,
        change: ServerChange,
    ) -> Result<(), ManagementError> {
        self.check_permission()?;
        if !valid_name(&name) {
            return Err(invalid(
                "Use letters, numbers, underscores or hyphens for the server name",
            ));
        }
        let _lock = self.editing.lock().await;
        let original = tokio::fs::read(&self.path).await.map_err(internal)?;
        let mut raw: Value = serde_json::from_slice(&original).map_err(internal)?;
        let mut config: Config = serde_json::from_value(raw.clone()).map_err(internal)?;
        if config.definitions().get(&name) != change.expected.as_ref() {
            return Err(changed());
        }
        if change.expected == change.replacement {
            return Ok(());
        }
        if let Some(next) = &change.replacement {
            let tools = self.discover_tools(&next.launch).await?;
            for b in &next.bindings {
                if !valid_name(&b.skill)
                    || !valid_name(&b.name)
                    || self.reserved_skills.contains(&b.skill)
                {
                    return Err(invalid(format!(
                        "Invalid skill or tool name {}.{}",
                        b.skill, b.name
                    )));
                }
                if !tools.iter().any(|t| t["name"] == b.tool) {
                    return Err(invalid(format!("Server has no tool {}", b.tool)));
                }
            }
        }
        config.replace_server(&name, change)?;
        raw["mcp_servers"] = serde_json::to_value(config.mcp_servers).map_err(internal)?;
        raw["skills"] = serde_json::to_value(config.skills).map_err(internal)?;
        raw["defaults"]["tool_grants"] =
            serde_json::to_value(config.defaults.tool_grants).map_err(internal)?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || persist(&path, &original, &raw))
            .await
            .map_err(internal)?
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}
fn changed() -> ManagementError {
    ManagementError(
        StatusCode::CONFLICT,
        "Configuration changed. Reload the server before saving.".into(),
    )
}

impl ToolConfig {
    fn local_name(&self) -> &str {
        match self {
            Self::TimerWait => "wait",
            Self::SendMessage => "send",
            Self::WaitForMessage => "receive",
            Self::Mcp { tool, name, .. } => name.as_deref().unwrap_or(tool),
            #[cfg(feature = "sandbox")]
            Self::SandboxCommand { name, .. } => name,
        }
    }
}
impl Config {
    fn replace_server(&mut self, name: &str, change: ServerChange) -> Result<(), ManagementError> {
        let mut emptied = BTreeSet::new();
        for (skill, config) in &mut self.skills {
            let before = config.tools.len();
            config
                .tools
                .retain(|tool| !matches!(tool,ToolConfig::Mcp{server,..} if server==name));
            if before > 0 && config.tools.is_empty() {
                emptied.insert(skill.clone());
            }
        }
        if let Some(old) = change.expected {
            for b in old.bindings {
                self.defaults
                    .tool_grants
                    .remove(&format!("{}.{}", b.skill, b.name));
            }
        }
        self.mcp_servers.remove(name);
        if let Some(next) = change.replacement {
            self.mcp_servers.insert(name.into(), next.launch);
            for b in next.bindings {
                let skill = self
                    .skills
                    .entry(b.skill.clone())
                    .or_insert_with(|| SkillConfig {
                        description: format!("Tools for {}", b.skill),
                        tools: vec![],
                    });
                if skill.tools.iter().any(|t| t.local_name() == b.name) {
                    return Err(invalid(format!(
                        "Tool {}.{} already exists",
                        b.skill, b.name
                    )));
                }
                self.defaults
                    .tool_grants
                    .insert(format!("{}.{}", b.skill, b.name), b.grant);
                skill.tools.push(ToolConfig::Mcp {
                    server: name.into(),
                    tool: b.tool,
                    name: Some(b.name),
                });
                emptied.remove(&b.skill);
            }
        }
        for skill in emptied {
            self.skills.remove(&skill);
        }
        Ok(())
    }
}
fn persist(path: &Path, original: &[u8], raw: &Value) -> Result<(), ManagementError> {
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().ok_or_else(|| internal("no parent"))?)
            .map_err(internal)?;
    serde_json::to_writer_pretty(&mut file, raw).map_err(internal)?;
    file.write_all(b"\n").map_err(internal)?;
    file.as_file()
        .set_permissions(fs::metadata(path).map_err(internal)?.permissions())
        .map_err(internal)?;
    file.as_file().sync_all().map_err(internal)?;
    if fs::read(path).map_err(internal)? != original {
        return Err(changed());
    }
    file.persist(path).map_err(internal)?;
    Ok(())
}
