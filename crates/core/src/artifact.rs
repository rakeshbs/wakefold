//! Publication and retrieval of immutable files produced in chat workspaces.

use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::thread;

use async_trait::async_trait;
use cap_std::{ambient_authority, fs::Dir};
use futures_channel::oneshot;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

use crate::{
    ChatId, JsonValue, RunWorkspace, Tool, ToolContext, ToolError, ToolOutput, ToolRecovery,
    ToolSpec, WorkspaceStore,
};

const FILE_DIRECTORY: &str = "artifact-files";

/// The metadata of a single published revision.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct ArtifactRevision {
    /// The stable identity shared by all revisions of the deliverable.
    pub artifact_id: i64,
    /// The one-based revision number within this artifact.
    pub revision: u64,
    /// The label supplied by the publishing agent.
    pub title: String,
    /// The basename of the published file.
    pub filename: String,
    /// The media type inferred from the filename.
    pub media_type: String,
    /// The length in bytes of the published copy.
    pub size_bytes: u64,
    /// The agent that published this revision.
    pub agent_id: i64,
}

/// An immutable revision and the specific run that published it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublishedArtifactRevision {
    /// The publishing run, including its identity within a multi-turn chat.
    pub run_id: i64,
    /// The metadata of the file at publication time.
    #[serde(flatten)]
    pub artifact: ArtifactRevision,
}

/// A request to publish an existing file from the run's chat workspace.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPublication {
    title: String,
    path: PathBuf,
    artifact_id: Option<i64>,
}

impl ArtifactPublication {
    /// Describe a new artifact from a file relative to its chat workspace.
    pub fn new(title: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            title: title.into(),
            path: path.into(),
            artifact_id: None,
        }
    }

    /// Publish a new revision of this existing artifact in the same chat.
    pub fn with_artifact_id(mut self, artifact_id: i64) -> Self {
        self.artifact_id = Some(artifact_id);
        self
    }
}

/// Publish an existing workspace file through a journaled run.
#[derive(Debug, Default, Clone, Copy)]
pub struct PublishArtifact;

#[async_trait]
impl Tool for PublishArtifact {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "publish",
            "Publish a finished file from the chat workspace as an artifact. Supply artifact_id to publish a new revision of an existing artifact.",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "path": {"type": "string", "description": "File path relative to the chat workspace root"},
                    "artifact_id": {"type": "integer", "minimum": 1}
                },
                "required": ["title", "path"],
                "additionalProperties": false
            }),
        )
        .with_recovery(ToolRecovery::RetryAfterCrash)
    }

    async fn call(&self, _: &JsonValue) -> Result<JsonValue, ToolError> {
        Err(ToolError::new("publication requires a journaled run"))
    }

    async fn call_with_output(
        &self,
        _: ToolContext,
        arguments: &JsonValue,
    ) -> Result<ToolOutput, ToolError> {
        ArtifactPublication::deserialize(arguments)
            .map(ToolOutput::PublishArtifact)
            .map_err(|source| ToolError::with_source("invalid artifact publication", source))
    }
}

/// A failure to publish or retrieve an artifact.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ArtifactError {
    /// The supplied title was empty or whitespace only.
    #[error("artifact title must be nonempty")]
    EmptyTitle,
    /// The source must point to a regular file inside the chat workspace.
    #[error("artifact source must be a regular file within its chat workspace")]
    InvalidSource,
    /// The requested artifact is unavailable in this chat.
    #[error("artifact {0} is unavailable in this chat")]
    UnknownArtifact(i64),
    /// A saved call key could not identify an artifact file.
    #[error("invalid artifact tool call key")]
    InvalidCallKey,
    /// The file copy worker stopped before returning a result.
    #[error("artifact copy worker stopped")]
    CopyWorkerStopped,
    /// SQLite could not read or save the artifact metadata.
    #[error("artifact metadata: {0}")]
    Database(#[from] rusqlite::Error),
    /// A source or published file could not be accessed.
    #[error("artifact filesystem: {0}")]
    Filesystem(#[from] io::Error),
}

pub(crate) struct SavedArtifact {
    publication: ArtifactPublication,
    filename: String,
    size_bytes: u64,
    tool_call_key: String,
}

pub(crate) fn directory(state: &Path) -> PathBuf {
    state.join(FILE_DIRECTORY)
}

fn copy_path(directory: &Path, agent_id: i64, call_key: &str) -> Result<PathBuf, ArtifactError> {
    if call_key.is_empty()
        || !call_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(ArtifactError::InvalidCallKey);
    }
    Ok(directory.join(format!("{agent_id}-{call_key}")))
}

impl ArtifactPublication {
    pub(crate) fn validate_target(
        &self,
        connection: &Connection,
        agent_id: i64,
    ) -> Result<(), ArtifactError> {
        if let Some(artifact_id) = self.artifact_id {
            let exists: bool = connection.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM artifacts a JOIN agent_sessions r USING (chat_id)
                    WHERE a.artifact_id = ?1 AND r.agent_id = ?2
                 )",
                params![artifact_id, agent_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(ArtifactError::UnknownArtifact(artifact_id));
            }
        }
        Ok(())
    }

    pub(crate) async fn save_copy(
        self,
        workspace: &RunWorkspace,
        directory: &Path,
        tool_call_key: &str,
    ) -> Result<SavedArtifact, ArtifactError> {
        let (sender, receiver) = oneshot::channel();
        let directory = directory.to_owned();
        let workspace = workspace.clone();
        let tool_call_key = tool_call_key.to_owned();
        thread::Builder::new()
            .name("wakefold-artifact-copy".into())
            .spawn(move || {
                let result = self.copy_file(&workspace, &directory, tool_call_key);
                let _ = sender.send(result);
            })?;
        receiver
            .await
            .map_err(|_| ArtifactError::CopyWorkerStopped)?
    }

    fn copy_file(
        mut self,
        workspace: &RunWorkspace,
        directory: &Path,
        tool_call_key: String,
    ) -> Result<SavedArtifact, ArtifactError> {
        let title = self.title.trim();
        if title.is_empty() {
            return Err(ArtifactError::EmptyTitle);
        }
        if title.len() != self.title.len() {
            self.title = title.to_owned();
        }
        let filename = self.path.file_name().and_then(|name| name.to_str());
        let Some(filename) = filename.filter(|_| {
            self.path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        }) else {
            return Err(ArtifactError::InvalidSource);
        };
        let filename = filename.to_owned();

        let agent_id = workspace.agent_id();
        let destination = copy_path(directory, agent_id, &tool_call_key)?;
        fs::create_dir_all(directory)?;
        match fs::metadata(&destination) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut source = open_source(workspace.chat_root(), &self.path)?;
                let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
                io::copy(&mut source, temporary.as_file_mut())?;
                temporary.as_file().sync_all()?;
                if let Err(error) = temporary.persist_noclobber(&destination)
                    && error.error.kind() != io::ErrorKind::AlreadyExists
                {
                    return Err(error.error.into());
                }
            }
            Err(error) => return Err(error.into()),
        }
        #[cfg(unix)]
        {
            File::open(directory)?.sync_all()?;
            if let Some(parent) = directory.parent() {
                File::open(parent)?.sync_all()?;
            }
        }
        let size_bytes = fs::metadata(&destination)?.len();
        tracing::info!(target: "wakefold::artifact", agent_id, "artifact copy saved");
        Ok(SavedArtifact {
            publication: self,
            filename,
            size_bytes,
            tool_call_key,
        })
    }
}

fn open_source(root: &Path, path: &Path) -> Result<File, ArtifactError> {
    // Resolve the source through an open directory to contain symlinks even
    // if the workspace changes between validation and opening the file.
    let directory = Dir::open_ambient_dir(root, ambient_authority())?;
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        // A special file may replace the source immediately before opening it.
        options.custom_flags(libc::O_NONBLOCK);
    }
    if !directory.metadata(path)?.is_file() {
        return Err(ArtifactError::InvalidSource);
    }
    let source = directory.open_with(path, &options)?.into_std();
    if !source.metadata()?.is_file() {
        return Err(ArtifactError::InvalidSource);
    }
    Ok(source)
}

impl SavedArtifact {
    pub(crate) fn record(
        self,
        connection: &Connection,
        agent_id: i64,
        run_id: i64,
    ) -> Result<ArtifactRevision, ArtifactError> {
        let Self {
            publication,
            filename,
            size_bytes,
            tool_call_key,
        } = self;
        let artifact_id = match publication.artifact_id {
            Some(id) => {
                publication.validate_target(connection, agent_id)?;
                id
            }
            None => connection.query_row(
                "INSERT INTO artifacts(chat_id)
                 SELECT chat_id FROM agent_sessions WHERE agent_id = ?1 RETURNING artifact_id",
                [agent_id],
                |row| row.get(0),
            )?,
        };
        let media_type = mime_guess::from_path(&filename)
            .first_or_octet_stream()
            .essence_str()
            .to_owned();
        let revision = connection.query_row(
            "INSERT INTO artifact_revisions
                (artifact_id, revision, title, filename, media_type, size_bytes, published_by_agent_id, tool_call_key, chat_id, published_in_run_id)
             VALUES (
                ?1, (SELECT COALESCE(MAX(revision), 0) + 1 FROM artifact_revisions WHERE artifact_id = ?1),
                ?2, ?3, ?4, ?5, ?6, ?7, (SELECT chat_id FROM agent_sessions WHERE agent_id=?6), ?8
             ) RETURNING revision",
            params![
                artifact_id, publication.title, filename, media_type, size_bytes, agent_id, tool_call_key, run_id
            ],
            |row| row.get(0),
        )?;
        Ok(ArtifactRevision {
            artifact_id,
            revision,
            title: publication.title,
            filename,
            media_type,
            size_bytes,
            agent_id,
        })
    }
}

fn read_revision(row: &Row<'_>) -> rusqlite::Result<ArtifactRevision> {
    Ok(ArtifactRevision {
        artifact_id: row.get(0)?,
        revision: row.get(1)?,
        title: row.get(2)?,
        filename: row.get(3)?,
        media_type: row.get(4)?,
        size_bytes: row.get(5)?,
        agent_id: row.get(6)?,
    })
}

impl WorkspaceStore {
    /// List the latest published revision of each artifact in the given chat.
    ///
    /// An unknown chat has an empty list.
    ///
    /// # Errors
    /// Returns an error if the artifact catalog could not be read.
    pub fn artifacts(&self, chat_id: ChatId) -> Result<Vec<ArtifactRevision>, ArtifactError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT a.artifact_id, r.revision, r.title, r.filename, r.media_type, r.size_bytes, r.published_by_agent_id
             FROM artifacts a
             JOIN artifact_revisions r ON r.artifact_id = a.artifact_id
             WHERE a.chat_id = ?1 AND r.revision = (
                SELECT MAX(revision) FROM artifact_revisions WHERE artifact_id = a.artifact_id
             )
             ORDER BY a.artifact_id",
        )?;
        let rows = statement.query_map([chat_id], read_revision)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// List all revisions in a chat, ordered by artifact and newest revision first.
    ///
    /// # Errors
    /// Returns an error if the catalog cannot be read.
    pub fn artifact_revisions(
        &self,
        chat: ChatId,
    ) -> Result<Vec<PublishedArtifactRevision>, ArtifactError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT artifact_id, revision, title, filename, media_type, size_bytes,
                    published_by_agent_id, published_in_run_id
             FROM artifact_revisions WHERE chat_id = ?1 ORDER BY artifact_id, revision DESC",
        )?;
        let rows = statement.query_map([chat], |row| {
            Ok(PublishedArtifactRevision {
                run_id: row.get(7)?,
                artifact: read_revision(row)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Open the exact published revision identified by this chat, artifact, and revision number.
    ///
    /// Returns `None` if the revision is unavailable in the chat.
    ///
    /// # Errors
    /// Returns an error if the catalog or a published file could not be accessed.
    pub fn open_artifact(
        &self,
        chat_id: ChatId,
        artifact_id: i64,
        revision: u64,
    ) -> Result<Option<(ArtifactRevision, File)>, ArtifactError> {
        let Ok(revision) = i64::try_from(revision) else {
            return Ok(None);
        };
        let publication = self.connection.query_row(
            "SELECT a.artifact_id, r.revision, r.title, r.filename, r.media_type, r.size_bytes, r.published_by_agent_id,
                    r.tool_call_key
             FROM artifact_revisions r JOIN artifacts a ON a.artifact_id = r.artifact_id
             WHERE a.chat_id = ?1 AND a.artifact_id = ?2 AND r.revision = ?3",
            params![chat_id, artifact_id, revision],
            |row| Ok((read_revision(row)?, row.get::<_, String>(7)?)),
        ).optional()?;
        let Some((publication, key)) = publication else {
            return Ok(None);
        };
        let path = copy_path(&directory(&self.state), publication.agent_id, &key)?;
        Ok(Some((publication, File::open(path)?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_uses_regular_files_in_the_chat_workspace() {
        let state = tempfile::tempdir().unwrap();
        let mut store = WorkspaceStore::open(state.path()).unwrap();
        let workspace = store
            .open_run_workspace(ChatId::new_v4(), "report")
            .unwrap();
        let artifact_files = directory(state.path());
        let attempt = |path| {
            ArtifactPublication::new("Report", path).copy_file(
                &workspace,
                &artifact_files,
                "attempt".into(),
            )
        };
        for path in ["../draft.csv", "agents"] {
            assert!(matches!(attempt(path), Err(ArtifactError::InvalidSource)));
        }
        #[cfg(unix)]
        {
            let outside = state.path().join("outside.csv");
            fs::write(&outside, "outside the chat").unwrap();
            std::os::unix::fs::symlink(outside, workspace.chat_root().join("linked.csv")).unwrap();
            assert!(attempt("linked.csv").is_err());
        }
        assert_eq!(fs::read_dir(artifact_files).unwrap().count(), 0);
    }
}
