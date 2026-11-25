//! Host-supplied skills and the tools activated during one run.

use std::collections::{BTreeMap, HashSet};
use std::fmt::{self, Write};

use serde_json::json;

use crate::{JsonValue, RunError, Tool, ToolCall, ToolContext, ToolError, ToolSpec};

/// A named group of tools the agent can load together.
///
/// The loader shows the model the skill's name and short description at startup.
/// Tool names are local to their skill. Skill and local tool names may contain
/// ASCII letters, digits, underscores, and hyphens. Wakefold reserves `skills`
/// for the built-in loader.
#[derive(Clone, Copy)]
pub struct Skill<'a> {
    name: &'a str,
    description: &'a str,
    tools: &'a [&'a dyn Tool],
}

impl<'a> Skill<'a> {
    /// Make these tools available to runs configured with this skill.
    pub fn new(name: &'a str, description: &'a str, tools: &'a [&'a dyn Tool]) -> Self {
        Self {
            name,
            description,
            tools,
        }
    }
}

impl fmt::Debug for Skill<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Skill")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("tool_count", &self.tools.len())
            .finish()
    }
}

const LOADER: &str = "skills.load";

pub(crate) struct RunSkills<'a> {
    // Activation moves each skill's entries into the advertised set exactly once.
    available: BTreeMap<&'a str, Vec<(ToolSpec, &'a dyn Tool)>>,
    pub(crate) advertised: Vec<ToolSpec>,
    handlers: BTreeMap<String, &'a dyn Tool>,
}

impl<'a> RunSkills<'a> {
    pub(crate) fn new(skills: &[Skill<'a>]) -> Result<Self, RunError> {
        let mut available = BTreeMap::from([("skills", Vec::new())]);
        let mut description =
            String::from("Load a skill by name to enable its tools on the next turn.");
        for skill in skills {
            validate_name(skill.name)?;
            let mut tools = Vec::with_capacity(skill.tools.len());
            let mut names = HashSet::new();
            for &tool in skill.tools {
                let mut spec = tool.spec();
                validate_name(&spec.name)?;
                spec.name = format!("{}.{}", skill.name, spec.name);
                if !names.insert(spec.name.clone()) {
                    return Err(RunError::DuplicateToolName { name: spec.name });
                }
                tools.push((spec, tool));
            }
            if available.insert(skill.name, tools).is_some() {
                return Err(RunError::DuplicateSkillName {
                    name: skill.name.to_owned(),
                });
            }
            write!(description, "\n{}: {}", skill.name, skill.description)
                .expect("writing a skill description into a String succeeds");
        }
        let loader = ToolSpec::new(
            LOADER,
            description,
            json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"],
                "additionalProperties": false
            }),
        );
        Ok(Self {
            available,
            advertised: vec![loader],
            handlers: BTreeMap::new(),
        })
    }

    pub(crate) fn can_call(&self, name: &str) -> bool {
        name == LOADER || self.handlers.contains_key(name)
    }

    pub(crate) async fn call(
        &mut self,
        call: &ToolCall,
        context: ToolContext,
    ) -> Result<JsonValue, ToolError> {
        if call.name == LOADER {
            return self.load(&call.arguments);
        }
        let handler = self
            .handlers
            .get(&call.name)
            .expect("tool names are validated before dispatch");
        handler.call_with_context(context, &call.arguments).await
    }

    fn load(&mut self, arguments: &JsonValue) -> Result<JsonValue, ToolError> {
        let name = arguments
            .as_object()
            .filter(|fields| fields.len() == 1)
            .and_then(|fields| fields.get("name"))
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                ToolError::new("expected an object with a single string field `name`")
            })?;
        let tools = self
            .available
            .get_mut(name)
            .ok_or_else(|| ToolError::new(format!("skill {name} is unavailable to this agent")))?;
        for (spec, tool) in tools.drain(..) {
            self.handlers.insert(spec.name.clone(), tool);
            self.advertised.push(spec);
        }
        Ok(json!({"loaded": name}))
    }
}

fn validate_name(name: &str) -> Result<(), RunError> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(RunError::InvalidName {
            name: name.to_owned(),
        });
    }
    Ok(())
}
