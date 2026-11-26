//! Agent decisions and the driver that executes or resumes a run.

mod recovery;
pub use recovery::RunCheckpoint;
use recovery::{RunDriver, RunPhase};

use std::collections::HashSet;
use std::future::Future;
use std::pin::pin;

use futures_util::future::{Either, select};

use thiserror::Error;

use crate::skill::RunSkills;
use crate::{
    CancellationToken, Message, ModelRequest, ModelRetryPolicy, Provider, ProviderError, RunEvent,
    RunJournal, RunWorkspace, Skill, TokenUsage, ToolCall, ToolCallId, ToolContext, ToolError,
};

/// Limits, retries, and cancellation for a single agent run.
#[derive(Debug, Clone)]
#[must_use]
pub struct RunOptions {
    max_model_turns: usize,
    token_budget: Option<u64>,
    context_byte_budget: Option<usize>,
    model_retry_policy: ModelRetryPolicy,
    workspace: Option<RunWorkspace>,
    cancellation: Option<CancellationToken>,
}

impl RunOptions {
    /// Set the maximum number of model turns with the default retry policy.
    pub fn new(max_model_turns: usize) -> Self {
        Self {
            max_model_turns,
            token_budget: None,
            context_byte_budget: None,
            model_retry_policy: ModelRetryPolicy::default(),
            workspace: None,
            cancellation: None,
        }
    }

    /// Stop further model calls and tool dispatch once observed usage reaches this limit.
    ///
    /// A model call can cross the limit before its usage is reported. A
    /// completed final answer is returned with its observed usage.
    pub fn with_token_budget(mut self, max_tokens: u64) -> Self {
        self.token_budget = Some(max_tokens);
        self
    }

    /// Limit the transcript and provider replay data used for model calls, in bytes.
    ///
    /// The budget counts message text, tool call IDs and names, and compact
    /// JSON for tool arguments, results, and provider replay data. When needed,
    /// the model summarizes older exchanges in a separate request. The
    /// original prompt, loaded tool definitions, and latest exchange are kept.
    /// Summary requests count toward token usage and have one opportunity per agent turn.
    pub fn with_context_byte_budget(mut self, max_bytes: usize) -> Self {
        self.context_byte_budget = Some(max_bytes);
        self
    }

    /// Configure retries for each model turn.
    ///
    /// Attempts for the same model turn share one slot in the turn limit.
    pub fn with_model_retry_policy(mut self, policy: ModelRetryPolicy) -> Self {
        self.model_retry_policy = policy;
        self
    }

    /// Make this persistent workspace available to the run's trusted host tools.
    pub fn with_workspace(mut self, workspace: RunWorkspace) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Stop the run when the caller cancels this token.
    ///
    /// The token is also available to tools through [`ToolContext`]. The
    /// driver drops an in-flight provider call, retry delay, or tool call when
    /// cancellation interrupts it.
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation = Some(token);
        self
    }
}

/// The provider-reported usage accumulated during a run.
#[derive(serde::Serialize, serde::Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunUsage {
    /// Tokens reported by the provider, including for compaction summaries.
    #[serde(rename = "reported")]
    pub reported_tokens: TokenUsage,
    /// Completed model calls without token counts, including compaction summaries.
    #[serde(rename = "unreported_turns")]
    pub model_calls_without_usage: usize,
}

impl RunUsage {
    fn record(&mut self, usage: Option<TokenUsage>) -> Result<(), RunError> {
        match usage {
            Some(usage) => {
                self.reported_tokens = self
                    .reported_tokens
                    .checked_add(usage)
                    .ok_or(RunError::TokenUsageOverflow)?;
            }
            None => self.model_calls_without_usage += 1,
        }
        Ok(())
    }

    fn check_budget(self, budget: Option<u64>) -> Result<(), RunError> {
        let Some(limit) = budget else {
            return Ok(());
        };
        if self.model_calls_without_usage > 0 {
            return Err(RunError::TokenUsageUnavailable { usage: self });
        }
        if self.reported_tokens.total_tokens() >= u128::from(limit) {
            return Err(RunError::TokenBudgetReached { limit, usage: self });
        }
        Ok(())
    }
}

/// The final answer and observed token usage for a completed run.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunReport {
    /// The final answer to the user.
    pub answer: String,
    /// Usage reported by the model provider across the run.
    pub usage: RunUsage,
}

/// The reason an agent run stopped before producing a final answer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RunError {
    /// The journal could not be read or advanced at the expected sequence.
    #[error(transparent)]
    Journal(#[from] crate::JournalError),

    /// The recorded events do not form a valid run history.
    #[error("cannot resume run: {0}")]
    InvalidJournal(&'static str),

    /// The supplied prompt differs from the one that started this run.
    #[error("the supplied prompt differs from the run's original prompt")]
    ResumePromptMismatch,

    /// A skill loaded by the saved run is missing from the supplied catalog.
    #[error("cannot resume run without the previously loaded skill {name}")]
    MissingSkillOnResume {
        /// The skill to supply when resuming this run.
        name: String,
    },

    /// The supplied workspace belongs to a different run.
    #[error("the supplied workspace belongs to a different run")]
    RunWorkspaceMismatch,

    /// Recovery found an application tool start without its committed result.
    #[error("outcome of tool {name} (call {id}) is unknown")]
    ToolOutcomeUnknown {
        /// The interrupted call.
        id: ToolCallId,
        /// The tool whose effects may have occurred.
        name: String,
    },

    /// The run was cancelled.
    #[error("agent run was cancelled")]
    Cancelled,

    /// The provider could not supply the next model turn.
    #[error("model call failed: {0}")]
    Provider(#[from] ProviderError),

    /// Two skills have the same name.
    #[error("multiple skills registered as {name}")]
    DuplicateSkillName {
        /// The repeated skill name.
        name: String,
    },

    /// A skill or tool name cannot be used as part of a qualified name.
    #[error("invalid skill or tool name {name:?}")]
    InvalidName {
        /// The invalid name.
        name: String,
    },

    /// Two tools advertised the same name.
    #[error("multiple tools registered as {name}")]
    DuplicateToolName {
        /// The repeated tool name.
        name: String,
    },

    /// The requested function is unknown or its tool is unavailable in the loaded skills.
    #[error("tool {name} is unavailable")]
    ToolUnavailable {
        /// The requested tool name.
        name: String,
    },

    /// A tool call ID was reused within this run, including within a single batch.
    #[error("tool call ID {id} was reused within this run")]
    DuplicateToolCallId {
        /// The repeated call identifier.
        id: ToolCallId,
    },

    /// A tool failed while executing the model's request.
    #[error("tool {name} failed: {source}")]
    Tool {
        /// The tool that failed.
        name: String,
        /// The failure reported by the tool.
        #[source]
        source: ToolError,
    },

    /// The run used all its model turns without a final answer.
    #[error("agent reached its model turn limit of {limit}")]
    TurnLimitReached {
        /// Maximum number of model turns allowed in the run.
        limit: usize,
    },

    /// The observed usage reached the configured token budget.
    #[error("agent reached its token budget of {limit}")]
    TokenBudgetReached {
        /// The configured token budget.
        limit: u64,
        /// Usage observed when the run stopped.
        usage: RunUsage,
    },

    /// The transcript payload exceeds the configured byte budget.
    #[error(
        "the transcript requires {required_bytes} bytes, exceeding the {limit_bytes}-byte budget"
    )]
    ContextBudgetExceeded {
        /// Configured transcript budget in bytes.
        limit_bytes: usize,
        /// Transcript payload size in bytes when the run stopped.
        required_bytes: u128,
    },

    /// The run needed another action but had incomplete token counts.
    #[error("provider omitted token usage required to continue a budgeted run")]
    TokenUsageUnavailable {
        /// Known counts and the number of model calls without token counts.
        usage: RunUsage,
    },

    /// The reported counts exceeded the supported range.
    #[error("reported token usage exceeds the supported range")]
    TokenUsageOverflow,
}

/// Run a single agent until it returns a final answer or reaches a limit.
///
/// The model sees the fixed `load_skill` and `call_tool` functions. It calls `load_skill`
/// to receive its tool definitions, then passes a qualified `tool_name` and
/// the tool's arguments to `call_tool`.
///
/// If the last allowed model turn requests tools, the run stops before
/// executing them because no follow-up turn remains.
///
/// # Errors
///
/// Returns [`RunError`] if the provider or tool fails, the model requests an
/// invalid tool call, registration is ambiguous, or the turn limit is exhausted.
pub async fn run(
    prompt: impl Into<String> + Send,
    provider: &dyn Provider,
    skills: &[Skill<'_>],
    max_model_turns: usize,
) -> Result<String, RunError> {
    run_streaming(prompt, provider, skills, max_model_turns, |_| {}).await
}

/// Run an agent while delivering model text as it arrives.
///
/// The callback receives text from each turn, including any text accompanying
/// tool calls. Tools run after the provider returns a completed turn. The
/// returned string is the text of the final turn.
///
/// # Errors
///
/// Returns [`RunError`] under the same conditions as [`run`].
pub async fn run_streaming(
    prompt: impl Into<String> + Send,
    provider: &dyn Provider,
    skills: &[Skill<'_>],
    max_model_turns: usize,
    on_text: impl FnMut(&str) + Send,
) -> Result<String, RunError> {
    let options = RunOptions::new(max_model_turns);
    Ok(
        run_streaming_with_options(prompt, provider, skills, options, on_text)
            .await?
            .answer,
    )
}

/// Run an agent with configured limits and return its answer and usage.
///
/// # Errors
///
/// Returns [`RunError`] if the run fails, is cancelled, or needs more work
/// after a limit is reached. A budgeted run also stops if usage is missing
/// and work remains.
pub async fn run_with_options(
    prompt: impl Into<String> + Send,
    provider: &dyn Provider,
    skills: &[Skill<'_>],
    options: RunOptions,
) -> Result<RunReport, RunError> {
    run_streaming_with_options(prompt, provider, skills, options, |_| {}).await
}

/// Run an agent with configured limits while reporting model text as it arrives.
///
/// # Errors
///
/// Returns [`RunError`] under the same conditions as [`run_with_options`].
pub async fn run_streaming_with_options(
    prompt: impl Into<String> + Send,
    provider: &dyn Provider,
    skills: &[Skill<'_>],
    options: RunOptions,
    on_text: impl FnMut(&str) + Send,
) -> Result<RunReport, RunError> {
    drive(prompt.into(), provider, skills, options, on_text, None).await
}

impl RunJournal<'_> {
    /// Start or resume this run with its persistent workspace and skill catalog.
    ///
    /// Supply the original prompt and skill definitions on restart. Turn limits
    /// and usage apply to the entire run. A finished run returns its saved answer.
    /// SQLite reads and writes are synchronous at step boundaries. A model request
    /// interrupted before its response was committed may be sent again. If a tool
    /// call was recorded but has no saved result, its [`crate::ToolRecovery`] policy
    /// applies. This includes resuming after a handler error in the same process.
    ///
    /// ```no_run
    /// # async fn example(provider: &dyn wakefold::Provider, skills: &[wakefold::Skill<'_>])
    /// # -> Result<(), Box<dyn std::error::Error>> {
    /// use wakefold::{RunOptions, WorkspaceStore};
    ///
    /// let mut store = WorkspaceStore::open("state")?;
    /// let mut journal = store.open_journal("chat-7", "run-1")?;
    /// let report = journal.run_or_resume("Set a timer", provider, skills, RunOptions::new(6)).await?;
    /// println!("{}", report.answer);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`RunError::ResumePromptMismatch`] if the original prompt differs
    /// and [`RunError::RunWorkspaceMismatch`] if the options specify another run's workspace.
    /// Returns [`RunError::MissingSkillOnResume`] if the catalog omits a previously loaded skill.
    /// A concurrent driver receives [`crate::JournalError::AlreadyRunning`] inside
    /// [`RunError::Journal`]. Returns [`RunError::ToolOutcomeUnknown`] if an interrupted
    /// tool cannot safely retry under its recorded and current recovery policies.
    /// Other failures are reported as in [`run_with_options`].
    pub async fn run_or_resume(
        &mut self,
        prompt: impl Into<String> + Send,
        provider: &dyn Provider,
        skills: &[Skill<'_>],
        options: RunOptions,
    ) -> Result<RunReport, RunError> {
        if options
            .workspace
            .as_ref()
            .is_some_and(|other| other != &self.workspace)
        {
            return Err(RunError::RunWorkspaceMismatch);
        }
        let _driver_lock = self.lock_driver()?;
        let options = options.with_workspace(self.workspace.clone());
        drive(prompt.into(), provider, skills, options, |_| {}, Some(self)).await
    }
}

async fn drive(
    prompt: String,
    provider: &dyn Provider,
    skills: &[Skill<'_>],
    options: RunOptions,
    mut on_text: impl FnMut(&str) + Send,
    journal: Option<&mut RunJournal<'_>>,
) -> Result<RunReport, RunError> {
    let cancellation = options.cancellation.as_ref();
    check_cancelled(cancellation)?;
    let mut emit_text = |chunk: &str| {
        if check_cancelled(cancellation).is_ok() {
            on_text(chunk);
        }
    };

    let mut run = RunDriver::open(prompt, journal, RunSkills::new(skills)?, &options)?;
    let mut context_sizes = crate::context::ContextSizes::default();
    loop {
        check_cancelled(cancellation)?;
        if let RunPhase::Finished(answer) = run.phase {
            return Ok(RunReport {
                answer,
                usage: run.state.usage,
            });
        }
        let action = next_action(
            &run.state.messages,
            options
                .max_model_turns
                .saturating_sub(run.state.model_turns),
        )
        .ok_or(RunError::TurnLimitReached {
            limit: options.max_model_turns,
        })?;
        match action {
            Action::CallModel => {
                run.state.usage.check_budget(options.token_budget)?;
                if matches!(run.phase, RunPhase::Ready)
                    && let Some(limit) = options.context_byte_budget
                    && let Some(compaction) = context_sizes.prepare(
                        &run.state.messages,
                        &run.state.assistant_replay_data,
                        limit,
                    )?
                {
                    let summary = crate::retry::stream_turn(
                        provider,
                        compaction.request(),
                        &mut |_| {},
                        options.model_retry_policy,
                        cancellation,
                    )
                    .await?;
                    run.compact(compaction, summary)?;
                    run.state.usage.check_budget(options.token_budget)?;
                }
                if !matches!(run.phase, RunPhase::ModelRequested) {
                    run.commit([RunEvent::ModelCallRequested])?;
                }
                let turn = crate::retry::stream_turn(
                    provider,
                    ModelRequest {
                        messages: &run.state.messages,
                        tools: &run.skills.built_in_tool_specs,
                        assistant_replay_data: &run.state.assistant_replay_data,
                    },
                    &mut emit_text,
                    options.model_retry_policy,
                    cancellation,
                )
                .await?;
                run.commit([RunEvent::ModelCallCompleted(turn)])?;
            }
            Action::RunTool(call) => {
                run.state.usage.check_budget(options.token_budget)?;
                if matches!(run.phase, RunPhase::Ready) {
                    run.prepare_tool_call(call.clone())?;
                    continue;
                }
                let context = ToolContext::new(
                    cancellation.cloned(),
                    options.workspace.clone(),
                    run.tool_call_key().map(str::to_owned),
                );
                let events = await_cancellable(run.skills.call(call, context), cancellation)
                    .await?
                    .map_err(|source| RunError::Tool {
                        name: run.skills.target_name(call).to_owned(),
                        source,
                    })?;
                run.commit(events)?;
            }
            Action::Finish(answer) => run.commit([RunEvent::RunFinished(answer.to_owned())])?,
        }
    }
}

fn check_cancelled(cancellation: Option<&CancellationToken>) -> Result<(), RunError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(RunError::Cancelled);
    }
    Ok(())
}

pub(crate) async fn await_cancellable<T>(
    operation: impl Future<Output = T>,
    cancellation: Option<&CancellationToken>,
) -> Result<T, RunError> {
    let Some(token) = cancellation else {
        return Ok(operation.await);
    };
    check_cancelled(cancellation)?;

    // Poll in-flight work first so a cooperative handler can observe the signal.
    match select(pin!(operation), pin!(token.cancelled())).await {
        Either::Left((result, _)) => {
            check_cancelled(cancellation)?;
            Ok(result)
        }
        Either::Right(_) => Err(RunError::Cancelled),
    }
}

enum Action<'a> {
    CallModel,
    RunTool(&'a ToolCall),
    Finish(&'a str),
}

fn next_action(messages: &[Message], remaining_turns: usize) -> Option<Action<'_>> {
    let completed_calls = messages
        .iter()
        .rev()
        .take_while(|message| matches!(message, Message::Tool(_)))
        .count();
    let turn = messages[..messages.len() - completed_calls].last()?;

    match turn {
        Message::Assistant(response) if response.tool_calls().is_empty() => {
            Some(Action::Finish(response.text().unwrap_or_default()))
        }
        _ if remaining_turns == 0 => None,
        Message::Assistant(response) => Some(
            response
                .tool_calls()
                .get(completed_calls)
                .map_or(Action::CallModel, Action::RunTool),
        ),
        Message::User(_) | Message::Summary(_) | Message::Tool(_) => Some(Action::CallModel),
    }
}

fn validate_calls(
    calls: &[ToolCall],
    used_call_ids: &mut HashSet<ToolCallId>,
    skills: &RunSkills<'_>,
) -> Result<(), RunError> {
    for call in calls {
        skills.validate_call(call)?;
        if !used_call_ids.insert(call.id.clone()) {
            return Err(RunError::DuplicateToolCallId {
                id: call.id.clone(),
            });
        }
    }
    Ok(())
}
