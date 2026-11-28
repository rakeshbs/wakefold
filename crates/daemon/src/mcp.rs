//! MCP tools backed by a local stdio server.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientConfig, Implementation,
    Tool as RemoteTool,
};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{Peer, RoleClient, ServiceExt};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::timeout;
use wakefold::{JsonValue, Tool, ToolError, ToolSpec};

use crate::DaemonError;

const START_TIMEOUT: Duration = Duration::from_secs(15);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(7);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServerConfig {
    pub(crate) command: PathBuf,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    #[serde(default)]
    pub(crate) inherit_env: Vec<String>,
}

pub(crate) struct McpServer {
    service: RunningService<RoleClient, ClientConfig>,
    tools: BTreeMap<String, RemoteTool>,
}

impl McpServer {
    pub(crate) async fn start(
        name: &str,
        config: &ServerConfig,
        directory: &Path,
    ) -> Result<Self, DaemonError> {
        if config.command.as_os_str().is_empty()
            || config
                .inherit_env
                .iter()
                .any(|key| key.is_empty() || key.contains(['=', '\0']))
        {
            return Err("Enter a server executable and valid environment variable names".into());
        }
        let command = if config.command.is_relative() && config.command.components().count() > 1 {
            directory.join(&config.command)
        } else {
            config.command.clone()
        };
        let mut command = Command::new(command);
        command
            .args(&config.args)
            .current_dir(directory)
            .env_clear()
            .kill_on_drop(true);
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        for variable in &config.inherit_env {
            let value = std::env::var_os(variable).ok_or_else(|| {
                format!("MCP server {name} requires environment variable {variable}")
            })?;
            command.env(variable, value);
        }

        let client = ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("wakefold", env!("CARGO_PKG_VERSION")),
        );
        let connect = async {
            let transport = TokioChildProcess::new(command)?;
            let service = client.serve(transport).await?;
            let discovered = service.list_all_tools().await?;
            let mut tools = BTreeMap::new();
            for tool in discovered {
                let remote_name = tool.name.to_string();
                if tools.insert(remote_name.clone(), tool).is_some() {
                    return Err(format!("server advertised {remote_name} more than once").into());
                }
            }
            Ok::<_, DaemonError>(Self { service, tools })
        };
        let server = timeout(START_TIMEOUT, connect)
            .await
            .map_err(|_| format!("MCP server {name} timed out during discovery"))?
            .map_err(|error| format!("could not initialize MCP server {name}: {error}"))?;
        tracing::info!(
            mcp_server = name,
            tool_count = server.tools.len(),
            "MCP tools discovered"
        );
        Ok(server)
    }

    pub(crate) fn discovered(&self) -> Vec<JsonValue> {
        self.tools.values().map(|tool| serde_json::json!({"name":tool.name, "description":tool.description, "parameters":tool.input_schema})).collect()
    }

    pub(crate) fn tool(
        &self,
        server_name: &str,
        remote_name: String,
        local_name: Option<String>,
    ) -> Result<Box<dyn Tool>, DaemonError> {
        let definition = self
            .tools
            .get(&remote_name)
            .ok_or_else(|| format!("MCP server {server_name} has no tool {remote_name}"))?;
        let local_name = local_name.unwrap_or_else(|| remote_name.clone());
        if local_name.is_empty()
            || !local_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(format!("MCP tool {remote_name} needs a Wakefold name using ASCII letters, numbers, underscores, or hyphens").into());
        }
        // Remote recovery hints cannot make a call safe to replay after a crash.
        let spec = ToolSpec::new(
            local_name,
            definition.description.as_deref().unwrap_or_default(),
            JsonValue::Object(definition.input_schema.as_ref().clone()),
        );
        Ok(Box::new(McpTool {
            client: self.service.peer().clone(),
            remote_name,
            spec,
        }))
    }

    pub(crate) async fn close(&mut self, name: &str) {
        if let Err(error) = self.service.close_with_timeout(STOP_TIMEOUT).await {
            tracing::warn!(mcp_server = name, %error, "MCP server shutdown failed");
        }
    }
}

struct McpTool {
    client: Peer<RoleClient>,
    remote_name: String,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn call(&self, arguments: &JsonValue) -> Result<JsonValue, ToolError> {
        let arguments = arguments
            .as_object()
            .cloned()
            .ok_or_else(|| ToolError::new("MCP arguments must be a JSON object"))?;
        let request =
            CallToolRequestParams::new(self.remote_name.clone()).with_arguments(arguments);
        let response = timeout(CALL_TIMEOUT, self.client.call_tool_once(request))
            .await
            .map_err(|source| ToolError::with_source("MCP tool call timed out", source))?
            .map_err(|source| ToolError::with_source("MCP tool call failed", source))?;
        let CallToolResponse::Complete(mut result) = response else {
            return Err(ToolError::new(
                "MCP tool returned a task or requested additional input",
            ));
        };
        result.meta = None;
        serde_json::to_value(result).map_err(|source| {
            ToolError::with_source("could not serialize the MCP tool result", source)
        })
    }
}
