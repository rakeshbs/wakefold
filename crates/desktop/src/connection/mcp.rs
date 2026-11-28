//! The operator-facing MCP configuration API.
use super::Daemon;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize)]
pub(crate) struct Inventory {
    pub editable: bool,
    pub servers: Vec<Server>,
}
#[derive(Deserialize)]
pub(crate) struct Server {
    pub name: String,
    pub restart_required: bool,
    pub configuration: Option<Definition>,
    pub bindings: Vec<Binding>,
    pub tools: Vec<RemoteTool>,
}
#[derive(Clone, Deserialize)]
pub(crate) struct RemoteTool {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Value,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Launch {
    pub command: String,
    pub args: Vec<String>,
    pub inherit_env: Vec<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Definition {
    pub launch: Launch,
    pub bindings: Vec<Binding>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Binding {
    pub tool: String,
    pub skill: String,
    pub name: String,
    pub grant: Grant,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Grant {
    Allow,
    RequireApproval,
}
impl Grant {
    pub fn label(self) -> &'static str {
        match self {
            Self::Allow => "Allow",
            Self::RequireApproval => "Require approval",
        }
    }
}
impl Daemon {
    pub async fn discover_tools(&self, launch: &Launch) -> Result<Vec<RemoteTool>> {
        Ok(self
            .post(
                self.url.join("mcp/discover")?,
                serde_json::to_value(launch)?,
            )
            .await?
            .json()
            .await?)
    }
    pub async fn configure_server(
        &self,
        name: &str,
        expected: Option<Definition>,
        replacement: Option<Definition>,
    ) -> Result<()> {
        let mut url = self.url.join("mcp/servers")?;
        url.path_segments_mut()
            .expect("HTTP URL supports path segments")
            .push(name);
        self.post(url, json!({"expected":expected,"replacement":replacement}))
            .await?;
        Ok(())
    }
}
