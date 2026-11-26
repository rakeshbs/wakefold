//! Host-supplied skills and the tools activated during one run.

use std::collections::{BTreeMap, HashSet};
use std::fmt::{self, Write};

use serde_json::json;

use crate::{
    Effect, JsonValue, RunError, RunEvent, Tool, ToolCall, ToolContext, ToolError, ToolResult,
    ToolSpec,
};

/// A named group of tools the agent can load together.
///
/// The loader shows the model the skill's name and short description at startup.
/// Tool names are local to their skill. Skill and local tool names may contain
/// ASCII letters, digits, underscores, and hyphens. Wakefold reserves `skills`
/// for compatibility with the built-in loader in earlier journals.
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

pub(crate) const LOADER: &str = "load_skill";
pub(crate) const DISPATCHER: &str = "call_tool";

pub(crate) fn is_loader(name: &str) -> bool {
    matches!(name, LOADER | "skills.load")
}

pub(crate) struct RunSkills<'a> {
    available: BTreeMap<&'a str, Vec<(ToolSpec, &'a dyn Tool)>>,
    loaded: Vec<String>,
    pub(crate) advertised: Vec<ToolSpec>,
    handlers: BTreeMap<String, (Effect, &'a dyn Tool)>,
}

impl<'a> RunSkills<'a> {
    pub(crate) fn new(skills: &[Skill<'a>]) -> Result<Self, RunError> {
        let mut available = BTreeMap::from([("skills", Vec::new())]);
        let mut description = String::from(
            "Load a skill by name to receive its tool definitions. Use call_tool with a returned tool name and arguments on subsequent turns. Available skills:",
        );
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
        let dispatcher = ToolSpec::new(
            DISPATCHER,
            "Execute a tool from a loaded skill. Supply its qualified name and arguments matching the schema returned by load_skill.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "arguments": {}
                },
                "required": ["name", "arguments"],
                "additionalProperties": false
            }),
        );
        Ok(Self {
            available,
            loaded: Vec::new(),
            advertised: vec![loader, dispatcher],
            handlers: BTreeMap::new(),
        })
    }

    pub(crate) fn loaded(&self) -> &[String] {
        &self.loaded
    }

    pub(crate) fn validate_call(&self, call: &ToolCall) -> Result<(), RunError> {
        if is_loader(&call.name) {
            return Ok(());
        }
        let (name, _) = self.resolve(call).map_err(|source| RunError::Tool {
            name: call.name.clone(),
            source,
        })?;
        if !self.handlers.contains_key(name) {
            return Err(RunError::UnknownTool {
                name: name.to_owned(),
            });
        }
        Ok(())
    }

    pub(crate) fn definitions(&self, name: &str) -> Result<JsonValue, ToolError> {
        let tools = self.available.get(name).ok_or_else(|| unavailable(name))?;
        Ok(json!({
            "loaded": name,
            "tools": tools.iter().map(|(spec, _)| spec).collect::<Vec<_>>()
        }))
    }

    pub(crate) fn effect(&self, call: &ToolCall) -> Result<Effect, ToolError> {
        if is_loader(&call.name) {
            return Ok(Effect::ReadOnly);
        }
        let (name, _) = self.resolve(call)?;
        self.handlers
            .get(name)
            .map(|(effect, _)| *effect)
            .ok_or_else(|| ToolError::new(format!("tool {name} is unavailable to this agent")))
    }

    pub(crate) fn resolve<'call>(
        &self,
        call: &'call ToolCall,
    ) -> Result<(&'call str, &'call JsonValue), ToolError> {
        if call.name != DISPATCHER {
            // Direct names are retained for journals created before the dispatcher.
            return Ok((&call.name, &call.arguments));
        }
        let fields = call
            .arguments
            .as_object()
            .filter(|fields| fields.len() == 2)
            .ok_or_else(|| ToolError::new("expected name and arguments fields"))?;
        let name = fields
            .get("name")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| ToolError::new("expected a string tool name"))?;
        let arguments = fields
            .get("arguments")
            .ok_or_else(|| ToolError::new("expected tool arguments"))?;
        Ok((name, arguments))
    }

    pub(crate) async fn call(
        &self,
        call: &ToolCall,
        context: ToolContext,
    ) -> Result<Vec<RunEvent>, ToolError> {
        let mut events = Vec::new();
        let value = if is_loader(&call.name) {
            let name = load_name(&call.arguments)?;
            let definitions = self.definitions(name)?;
            if !self.is_loaded(name) {
                events.push(RunEvent::SkillLoaded(name.to_owned()));
            }
            definitions
        } else {
            let (name, arguments) = self.resolve(call)?;
            let (_, handler) = self.handlers.get(name).ok_or_else(|| {
                ToolError::new(format!("tool {name} is unavailable to this agent"))
            })?;
            handler.call_with_context(context, arguments).await?
        };
        events.push(RunEvent::ToolCallCompleted(ToolResult::new(
            call.id.clone(),
            value,
        )));
        Ok(events)
    }

    fn is_loaded(&self, name: &str) -> bool {
        name == "skills" || self.loaded.iter().any(|loaded| loaded == name)
    }

    pub(crate) fn activate(&mut self, name: &str) -> Result<(), ToolError> {
        if self.is_loaded(name) {
            return Ok(());
        }
        let tools = self.available.get(name).ok_or_else(|| unavailable(name))?;
        for (spec, tool) in tools {
            self.handlers
                .insert(spec.name.clone(), (spec.effect, *tool));
        }
        self.loaded.push(name.to_owned());
        Ok(())
    }
}

fn load_name(arguments: &JsonValue) -> Result<&str, ToolError> {
    arguments
        .as_object()
        .filter(|fields| fields.len() == 1)
        .and_then(|fields| fields.get("name"))
        .and_then(JsonValue::as_str)
        .ok_or_else(|| ToolError::new("expected an object with a single string field `name`"))
}

fn unavailable(name: &str) -> ToolError {
    ToolError::new(format!("skill {name} is unavailable to this agent"))
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
