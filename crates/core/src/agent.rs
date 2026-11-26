//! Agent decisions and the driver that executes or resumes a run.

mod recovery;
pub use recovery::RunCheckpoint;
use recovery::{Boundary, Driver};

use std::collections::HashSet;
use std::future::Future;
use std::pin::pin;

use futures_util::future::{Either, select};

use thiserror::Error;

use crate::skill::RunSkills;
use crate::{
    CallId, CancellationToken, Message, ModelRequest, Provider, ProviderError, RetryPolicy,
    RunEvent, RunJournal, RunWorkspace, Skill, TokenUsage, ToolCall, ToolContext, ToolError,
};

/// Limits, retries, and cancellation for a single agent run.
#[derive(Debug, Clone)]
#[must_use]
pub struct RunOptions {
    max_model_turns: usize,
    token_budget: Option<u64>,
    context_budget: Option<usize>,
    retry_policy: RetryPolicy,
    workspace: Option<RunWorkspace>,
    cancellation: Option<CancellationToken>,
}

impl RunOptions {
    /// Set the maximum number of model turns with the default retry policy.
    pub fn new(max_model_turns: usize) -> Self {
        Self {
            max_model_turns,
            token_budget: None,
            context_budget: None,
            retry_policy: RetryPolicy::default(),
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

    /// Limit the transcript and provider context used for model calls, in bytes.
    ///
    /// The budget counts message text, tool call IDs and names, and compact
    /// JSON for tool arguments, results, and provider context. When needed,
    /// the model summarizes older exchanges in a separate request. The
    /// original prompt, loaded tool definitions, and latest exchange are kept.
    /// Summary requests count toward token usage and have one opportunity per agent turn.
    pub fn with_context_budget(mut self, max_bytes: usize) -> Self {
        self.context_budget = Some(max_bytes);
        self
    }

    /// Configure retries for each model turn.
    ///
    /// Attempts for the same model turn share one slot in the turn limit.
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
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
    /// Sum of counts reported by completed model turns.
    pub reported: TokenUsage,
    /// Completed model turns for which the provider omitted usage.
    pub unreported_turns: usize,
}

impl RunUsage {
    fn record(&mut self, usage: Option<TokenUsage>) -> Result<(), RunError> {
        match usage {
            Some(usage) => {
                self.reported = self
                    .reported
                    .checked_add(usage)
                    .ok_or(RunError::TokenUsageOverflow)?;
            }
            None => self.unreported_turns += 1,
        }
        Ok(())
    }

    fn check_budget(self, budget: Option<u64>) -> Result<(), RunError> {
        let Some(limit) = budget else {
            return Ok(());
        };
        if self.unreported_turns > 0 {
            return Err(RunError::TokenUsageUnavailable { usage: self });
        }
        if self.reported.total_tokens() >= u128::from(limit) {
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

    /// The recorded history is inconsistent with the requested run.
    #[error("cannot resume run: {0}")]
    InvalidJournal(&'static str),

    /// Recovery found an application tool start without its committed result.
    #[error("outcome of tool {name} (call {id}) is unknown")]
    ToolOutcomeUnknown {
        /// The interrupted call.
        id: CallId,
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

    /// The model requested a tool that was not registered.
    #[error("model requested unknown tool {name}")]
    UnknownTool {
        /// The requested tool name.
        name: String,
    },

    /// The provider reused an ID for a tool call already answered in the transcript.
    #[error("model repeated tool call ID {id}")]
    DuplicateCallId {
        /// The repeated call identifier.
        id: CallId,
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

    /// The transcript payload is too large to send after compaction.
    #[error(
        "agent context requires {required} bytes after compaction, exceeding its {limit}-byte budget"
    )]
    ContextBudgetExceeded {
        /// Configured transcript budget in bytes.
        limit: usize,
        /// Transcript payload size in bytes when the run stopped.
        required: u128,
    },

    /// The run needed another action but had incomplete token counts.
    #[error("provider omitted token usage required to continue a budgeted run")]
    TokenUsageUnavailable {
        /// Known counts and the number of turns without reports.
        usage: RunUsage,
    },

    /// The reported counts exceeded the supported range.
    #[error("reported token usage exceeds the supported range")]
    TokenUsageOverflow,
}

/// Run a single agent until it returns a final answer or reaches a limit.
///
/// The model sees the fixed `load_skill` and `call_tool` functions. It calls `load_skill`
/// to receive its tool definitions, then uses `call_tool` to execute them.
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
    /// interrupted before its response was committed may be sent again. Interrupted
    /// tools that opt into retrying can reuse their saved idempotency key.
    ///
    /// ```no_run
    /// # async fn example(provider: &dyn wakefold::Provider, skills: &[wakefold::Skill<'_>])
    /// # -> Result<(), Box<dyn std::error::Error>> {
    /// use wakefold::{RunOptions, WorkspaceStore};
    ///
    /// let mut store = WorkspaceStore::open("state")?;
    /// let mut journal = store.open_journal("chat-7", "run-1")?;
    /// let report = journal.run("Set a timer", provider, skills, RunOptions::new(6)).await?;
    /// println!("{}", report.answer);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// A concurrent driver receives [`crate::JournalError::AlreadyRunning`] inside
    /// [`RunError::Journal`]. Returns [`RunError::ToolOutcomeUnknown`] if an interrupted
    /// tool cannot safely retry under its recorded and current recovery policies.
    /// Other failures are reported as in [`run_with_options`].
    pub async fn run(
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
            return Err(RunError::InvalidJournal(
                "workspace belongs to a different run",
            ));
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

    let mut run = Driver::open(prompt, journal, RunSkills::new(skills)?, &options)?;
    let mut context_sizes = crate::context::ContextSizes::default();
    loop {
        check_cancelled(cancellation)?;
        if let Boundary::Finished(answer) = run.boundary {
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
                if matches!(run.boundary, Boundary::Ready)
                    && let Some(limit) = options.context_budget
                    && let Some(compaction) = context_sizes.prepare(
                        &run.state.messages,
                        &run.state.assistant_context,
                        limit,
                    )?
                {
                    let summary = crate::retry::complete_turn(
                        provider,
                        compaction.request(),
                        &mut |_| {},
                        options.retry_policy,
                        cancellation,
                    )
                    .await?;
                    run.compact(compaction, summary)?;
                    run.state.usage.check_budget(options.token_budget)?;
                }
                if !matches!(run.boundary, Boundary::ModelRequested) {
                    run.commit([RunEvent::ModelCallRequested])?;
                }
                let turn = crate::retry::complete_turn(
                    provider,
                    ModelRequest {
                        messages: &run.state.messages,
                        tools: &run.skills.advertised,
                        assistant_context: &run.state.assistant_context,
                    },
                    &mut emit_text,
                    options.retry_policy,
                    cancellation,
                )
                .await?;
                run.commit([RunEvent::ModelCallCompleted(turn)])?;
            }
            Action::RunTool(call) => {
                run.state.usage.check_budget(options.token_budget)?;
                if matches!(run.boundary, Boundary::Ready) {
                    run.start_tool(call.clone())?;
                    continue;
                }
                let context = ToolContext::new(
                    cancellation.cloned(),
                    options.workspace.clone(),
                    run.idempotency_key().map(str::to_owned),
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
    used_call_ids: &mut HashSet<CallId>,
    skills: &RunSkills<'_>,
) -> Result<(), RunError> {
    for call in calls {
        skills.validate_call(call)?;
        if !used_call_ids.insert(call.id.clone()) {
            return Err(RunError::DuplicateCallId {
                id: call.id.clone(),
            });
        }
    }
    Ok(())
}
