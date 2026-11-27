//! Opt-in network test. Run with `--features openai --test live_openai_skills -- --ignored`.
#![cfg(feature = "openai")]

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use wakefold::{
    JsonValue, Message, ModelRequest, ModelResponse, ModelRetryPolicy, ModelTurn, OpenAiProvider,
    Provider, ProviderError, ReasoningEffort, RunEvent, RunOptions, Skill, Tool, ToolError,
    ToolSpec, WorkspaceStore,
};

const CHAT: wakefold::ChatId = wakefold::ChatId::from_u128(1);
const PROMPT: &str = "Retrieve the pickup code for order WF-206 from the order records. Reply with just the pickup code.";
const CHILD_STATE: &str = "WAKEFOLD_LIVE_ORDER_TEST";

struct Lookup<'a>(&'a Path);

#[async_trait]
impl Tool for Lookup<'_> {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "lookup",
            "Retrieve the pickup code for one order ID",
            json!({
                "type": "object", "properties": {"order_id": {"type": "string"}},
                "required": ["order_id"], "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: &JsonValue) -> Result<JsonValue, ToolError> {
        if args["order_id"] != "WF-206" {
            return Err(ToolError::new("unknown order"));
        }
        let code = fs::read_to_string(self.0.join("pickup-code"))
            .map_err(|e| ToolError::with_source("read test fixture", e))?;
        Ok(json!({"pickup_code": code}))
    }
}

struct LiveProvider {
    inner: OpenAiProvider,
    stop_after_activation: bool,
}

#[async_trait]
impl Provider for LiveProvider {
    async fn complete_response(
        &self,
        req: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        Ok(self.stream_turn(req, &mut |_| {}).await?.response)
    }

    async fn stream_turn(
        &self,
        req: ModelRequest<'_>,
        on_text: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        if self.stop_after_activation && req.messages.iter().any(|message| matches!(message, Message::Tool(result) if result.value["loaded"] == "orders")) {
            std::process::exit(73);
        }
        self.inner.stream_turn(req, on_text).await
    }
}

async fn drive(state: &Path, stop_after_activation: bool) -> Result<String, Box<dyn Error>> {
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-6-luna".into());
    let inner = OpenAiProvider::new(model, std::env::var("OPENAI_API_KEY")?)?;
    let provider = LiveProvider {
        inner,
        stop_after_activation,
    };
    let mut store = WorkspaceStore::open(state)?;
    let mut journal = store.open_journal(CHAT, "lookup")?;
    let tool = Lookup(state);
    let tools: [&dyn Tool; 1] = [&tool];
    let skills = [Skill::new(
        "orders",
        "Retrieve order records and pickup codes",
        &tools,
    )];
    let options = RunOptions::new(5)
        .with_token_budget(6_000)
        .with_model_retry_policy(ModelRetryPolicy::new(0, Duration::ZERO));
    let report = tokio::time::timeout(
        Duration::from_secs(90),
        journal.run_or_resume(PROMPT, &provider, &skills, options),
    )
    .await??;
    let entries = journal.load_after(0)?;
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(&entry.event, RunEvent::SkillLoaded(name) if name == "orders"))
            .count(),
        1
    );
    assert_eq!(entries.iter().filter(|entry| matches!(&entry.event, RunEvent::ToolCallPrepared { call, .. } if call.name == "call_tool" && call.arguments["tool_name"] == "orders.lookup")).count(), 1);
    Ok(report.answer)
}

#[tokio::test]
#[ignore = "uses a billable OpenAI model and requires OPENAI_API_KEY"]
async fn live_order_lookup_survives_a_process_restart() -> Result<(), Box<dyn Error>> {
    if let Some(state) = std::env::var_os(CHILD_STATE) {
        drive(Path::new(&state), true).await?;
        return Err("model finished before loading the orders skill".into());
    }
    let state = tempfile::tempdir()?;
    let code = format!("WF-{:016X}", rand::random::<u64>());
    fs::write(state.path().join("pickup-code"), &code)?;
    let status = Command::new(std::env::current_exe()?)
        .args([
            "--ignored",
            "--exact",
            "live_order_lookup_survives_a_process_restart",
            "--nocapture",
        ])
        .env(CHILD_STATE, state.path())
        .status()?;
    assert_eq!(
        status.code(),
        Some(73),
        "expected a committed skill activation in the first process"
    );
    assert_eq!(drive(state.path(), false).await?.trim(), code);
    Ok(())
}

const COMPACTION_CHILD: &str = "WAKEFOLD_LIVE_COMPACTION_TEST";
const COMPACTION_PROMPT: &str = "Process order WF-206. First load the orders skill. Call orders.history, orders.prepare, and orders.lookup in that order, in separate turns. Then load the verification skill and call verification.receipt with the pickup_code from orders.lookup. Reply with just the receipt. diagnostic_padding is irrelevant bookkeeping and should be summarized briefly.";

struct OrderStep {
    name: &'static str,
    padding_bytes: usize,
    next: &'static str,
}

#[async_trait]
impl Tool for OrderStep {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            self.name,
            "Complete this order workflow step before proceeding to the next tool",
            json!({
                "type": "object", "properties": {"order_id": {"type": "string"}},
                "required": ["order_id"], "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: &JsonValue) -> Result<JsonValue, ToolError> {
        if args["order_id"] != "WF-206" {
            return Err(ToolError::new("unknown order"));
        }
        Ok(json!({
            "completed": self.name,
            "next_tool": self.next,
            "diagnostic_padding": "x".repeat(self.padding_bytes)
        }))
    }
}

struct Receipt<'a>(&'a Path);

#[async_trait]
impl Tool for Receipt<'_> {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "receipt",
            "Retrieve the verification receipt using the pickup code from orders.lookup",
            json!({
                "type": "object", "properties": {"pickup_code": {"type": "string"}},
                "required": ["pickup_code"], "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: &JsonValue) -> Result<JsonValue, ToolError> {
        let expected = fs::read_to_string(self.0.join("pickup-code")).unwrap();
        if args["pickup_code"] != expected {
            return Err(ToolError::new("incorrect pickup code"));
        }
        Ok(json!({"receipt": fs::read_to_string(self.0.join("receipt")).unwrap()}))
    }
}

struct CompactionProvider {
    inner: OpenAiProvider,
    checkpoint: Option<JsonValue>,
    checked_resume: AtomicBool,
}

#[async_trait]
impl Provider for CompactionProvider {
    async fn complete_response(
        &self,
        req: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        Ok(self.stream_turn(req, &mut |_| {}).await?.response)
    }

    async fn stream_turn(
        &self,
        req: ModelRequest<'_>,
        on_text: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        if !req.tools.is_empty() {
            assert_eq!(
                req.tools
                    .iter()
                    .map(|spec| spec.name.as_str())
                    .collect::<Vec<_>>(),
                ["load_skill", "call_tool"]
            );
            if let Some(checkpoint) = &self.checkpoint {
                if !self.checked_resume.swap(true, Ordering::Relaxed) {
                    assert_eq!(
                        serde_json::to_value(req.messages).unwrap(),
                        checkpoint["state"]["messages"]
                    );
                    assert_eq!(
                        serde_json::to_value(req.assistant_replay_data).unwrap(),
                        checkpoint["state"]["assistant_context"]
                    );
                    eprintln!(
                        "Resumed model request exactly matches the saved compacted transcript and provider replay data."
                    );
                }
            } else if req
                .messages
                .iter()
                .any(|message| matches!(message, Message::Summary(_)))
            {
                // The compacted checkpoint and next model request are already durable.
                std::process::exit(74);
            }
        }
        self.inner.stream_turn(req, on_text).await
    }
}

async fn drive_compaction(
    state: &Path,
    checkpoint: Option<JsonValue>,
) -> Result<String, Box<dyn Error>> {
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-6-luna".into());
    let provider = CompactionProvider {
        inner: OpenAiProvider::new(model, std::env::var("OPENAI_API_KEY")?)?
            .with_reasoning_effort(ReasoningEffort::None),
        checkpoint,
        checked_resume: AtomicBool::new(false),
    };
    let history = OrderStep {
        name: "history",
        padding_bytes: 8_000,
        next: "orders.prepare",
    };
    let prepare = OrderStep {
        name: "prepare",
        padding_bytes: 16_000,
        next: "orders.lookup",
    };
    let lookup = Lookup(state);
    let receipt = Receipt(state);
    let order_tools: [&dyn Tool; 3] = [&history, &prepare, &lookup];
    let verification_tools: [&dyn Tool; 1] = [&receipt];
    let skills = [
        Skill::new(
            "orders",
            "Process orders and retrieve pickup codes",
            &order_tools,
        ),
        Skill::new(
            "verification",
            "Retrieve a receipt after the order lookup",
            &verification_tools,
        ),
    ];
    let mut store = WorkspaceStore::open(state)?;
    let mut journal = store.open_journal(CHAT, "compaction")?;
    let report = tokio::time::timeout(
        Duration::from_secs(180),
        journal.run_or_resume(
            COMPACTION_PROMPT,
            &provider,
            &skills,
            RunOptions::new(10)
                .with_context_byte_budget(22_000)
                .with_token_budget(50_000)
                .with_model_retry_policy(ModelRetryPolicy::new(0, Duration::ZERO)),
        ),
    )
    .await??;
    assert!(
        provider.checked_resume.load(Ordering::Relaxed),
        "the resumed request must be checked before contacting OpenAI"
    );
    Ok(report.answer)
}

#[tokio::test]
#[ignore = "uses a billable OpenAI model and requires OPENAI_API_KEY"]
async fn live_compaction_preserves_skills_across_restart() -> Result<(), Box<dyn Error>> {
    if let Some(state) = std::env::var_os(COMPACTION_CHILD) {
        drive_compaction(Path::new(&state), None).await?;
        return Err("model finished before transcript compaction".into());
    }
    let state = tempfile::tempdir()?;
    let receipt = format!("RECEIPT-{:016X}", rand::random::<u64>());
    fs::write(
        state.path().join("pickup-code"),
        format!("WF-{:016X}", rand::random::<u64>()),
    )?;
    fs::write(state.path().join("receipt"), &receipt)?;
    let status = Command::new(std::env::current_exe()?)
        .args([
            "--ignored",
            "--exact",
            "live_compaction_preserves_skills_across_restart",
            "--nocapture",
        ])
        .env(COMPACTION_CHILD, state.path())
        .status()?;
    assert_eq!(
        status.code(),
        Some(74),
        "the first process must exit after committing compaction"
    );

    let (checkpoint, checkpoint_sequence, definitions) = {
        let mut store = WorkspaceStore::open(state.path())?;
        let journal = store.open_journal(CHAT, "compaction")?;
        let entries = journal.load_after(0)?;
        let compacted = entries
            .iter()
            .find(|entry| matches!(entry.event, RunEvent::TranscriptCompacted(_)))
            .expect("a real compaction must have committed");
        let RunEvent::TranscriptCompacted(checkpoint) = &compacted.event else {
            unreachable!()
        };
        let definitions = entries
            .iter()
            .find_map(|entry| match &entry.event {
                RunEvent::ToolCallCompleted(result) if result.value["loaded"] == "orders" => {
                    Some(result.value.clone())
                }
                _ => None,
            })
            .expect("the model must load the orders skill before compaction");
        assert_eq!(definitions["tools"].as_array().unwrap().len(), 3);
        assert_eq!(definitions["tools"][2]["name"], "orders.lookup");
        assert_eq!(
            definitions["tools"][2]["parameters"],
            Lookup(state.path()).spec().argument_schema
        );
        (
            serde_json::to_value(checkpoint)?,
            compacted.sequence,
            definitions,
        )
    };
    let compacted_messages: Vec<Message> =
        serde_json::from_value(checkpoint["state"]["messages"].clone())?;
    assert!(
        compacted_messages.iter().any(
            |message| matches!(message, Message::Summary(summary) if !summary.trim().is_empty())
        )
    );
    assert!(
        compacted_messages
            .iter()
            .any(|message| matches!(message, Message::Tool(result) if result.value == definitions)),
        "the checkpoint must preserve the complete loader response"
    );
    assert!(!compacted_messages.iter().any(|message| matches!(message, Message::Tool(result) if result.value["completed"] == "history")), "the earlier history result must actually be summarized");
    eprintln!(
        "Compaction committed at journal sequence {checkpoint_sequence}; all three orders tool definitions survived unchanged."
    );

    assert_eq!(
        drive_compaction(state.path(), Some(checkpoint))
            .await?
            .trim(),
        receipt
    );
    let mut store = WorkspaceStore::open(state.path())?;
    let journal = store.open_journal(CHAT, "compaction")?;
    let entries = journal.load_after(0)?;
    let loaded: Vec<_> = entries
        .iter()
        .filter_map(|entry| match &entry.event {
            RunEvent::SkillLoaded(name) => Some((entry.sequence, name.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        loaded.iter().map(|(_, name)| *name).collect::<Vec<_>>(),
        ["orders", "verification"]
    );
    assert!(loaded[0].0 < checkpoint_sequence && loaded[1].0 > checkpoint_sequence);
    for (name, before_restart) in [
        ("orders.history", true),
        ("orders.prepare", true),
        ("orders.lookup", false),
        ("verification.receipt", false),
    ] {
        let calls: Vec<_> = entries.iter().filter(|entry| matches!(&entry.event,
            RunEvent::ToolCallPrepared { call, .. } if call.name == "call_tool" && call.arguments["tool_name"] == name)).collect();
        assert_eq!(calls.len(), 1, "{name} must execute exactly once");
        assert_eq!(
            calls[0].sequence < checkpoint_sequence,
            before_restart,
            "{name} must execute in the expected process"
        );
    }
    let verification = entries
        .iter()
        .find_map(|entry| match &entry.event {
            RunEvent::ToolCallCompleted(result) if result.value["loaded"] == "verification" => {
                Some(&result.value)
            }
            _ => None,
        })
        .expect("the new skill must return definitions after restart");
    assert_eq!(verification["tools"][0]["name"], "verification.receipt");
    assert_eq!(
        verification["tools"][0]["parameters"],
        Receipt(state.path()).spec().argument_schema
    );
    eprintln!(
        "After restart, call_tool dispatched orders.lookup, load_skill loaded verification, and call_tool dispatched verification.receipt successfully."
    );
    Ok(())
}
