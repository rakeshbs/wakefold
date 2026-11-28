//! HTTP snapshots and cursor-based WebSocket replay for one daemon.
pub(crate) mod mcp;
use crate::profiles::{LOCAL, Profile};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use reqwest::{
    Client, Url,
    header::{AUTHORIZATION, HeaderValue},
};
use rustls::{
    ClientConfig,
    pki_types::{CertificateDer, pem::PemObject},
};
use rustls_platform_verifier::BuilderVerifierExt;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{self, Instant},
};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{self, Message, client::IntoClientRequest},
};
use uuid::Uuid;

#[derive(Clone, Deserialize, PartialEq, Debug)]
pub struct SavedChat {
    pub chat_id: Uuid,
    pub root_agent_id: i64,
    pub title: Option<String>,
}
#[derive(Clone, Deserialize, PartialEq, Debug)]
pub struct Run {
    pub run_id: i64,
    pub agent_id: i64,
    pub parent_agent_id: Option<i64>,
    pub spawned_by_run_id: Option<i64>,
    pub run_key: String,
    pub latest_event: String,
    pub outcome: Option<String>,
    pub task: String,
    pub status: String,
}
impl Run {
    pub fn finished(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed")
    }
}
#[derive(Deserialize)]
struct AcceptedRun {
    run_id: i64,
}
#[derive(Clone, Deserialize, PartialEq, Debug)]
pub struct Approval {
    pub agent_id: i64,
    pub wait: Value,
}
#[derive(Clone, Deserialize, Debug, PartialEq)]
pub struct Event {
    pub sequence: u64,
    pub run_id: i64,
    pub event: EventBody,
}
#[derive(Clone, Deserialize, Debug, PartialEq)]
pub struct EventBody {
    pub kind: String,
    #[serde(default)]
    pub data: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ArtifactRevision {
    pub artifact_id: i64,
    pub revision: u64,
    pub run_id: i64,
    pub title: String,
    pub filename: String,
    pub size_bytes: u64,
}

pub enum Update {
    Snapshot {
        chats: Vec<SavedChat>,
        runs: Vec<Run>,
        approvals: Vec<Approval>,
        artifacts: Vec<ArtifactRevision>,
    },
    Event(Event),
    Reconnecting(String),
    Attention(String),
}
#[derive(Clone)]
pub struct Daemon {
    url: Url,
    http: Client,
    auth: Option<HeaderValue>,
    tls: Option<Arc<ClientConfig>>,
}
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

impl Daemon {
    pub fn new(profile: &Profile, token: Option<&str>, ca: Option<&Path>) -> Result<Self> {
        let url = Url::parse(&profile.url)?;
        let mut client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15));
        let (tls, auth) = if profile.url == LOCAL {
            ensure!(token.is_none(), "Local connection does not use a token.");
            (None, None)
        } else {
            Profile::remote(&profile.name, &profile.url)?;
            let token = token
                .filter(|s| !s.is_empty())
                .context("A daemon access token is required.")?;
            ensure!(
                token.bytes().all(|b| b.is_ascii_graphic()),
                "Invalid access token."
            );
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))?;
            value.set_sensitive(true);
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let builder = ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()?;
            let config = if let Some(path) = ca {
                let roots = CertificateDer::pem_file_iter(path)?.collect::<Result<Vec<_>, _>>()?;
                ensure!(!roots.is_empty(), "The CA file contains no certificates.");
                // Use the platform's chain and hostname verifier with the operator's private CA.
                let verifier =
                    rustls_platform_verifier::Verifier::new_with_extra_roots(roots, provider)?;
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(verifier))
            } else {
                builder.with_platform_verifier()?
            }
            .with_no_client_auth();
            client = client.tls_backend_preconfigured(config.clone());
            (Some(Arc::new(config)), Some(value))
        };
        Ok(Self {
            url,
            http: client.build()?,
            auth,
            tls,
        })
    }
    fn request(&self, path: &str) -> Result<reqwest::RequestBuilder> {
        let mut req = self.http.get(self.url.join(path)?);
        if let Some(auth) = &self.auth {
            req = req.header(AUTHORIZATION, auth);
        }
        Ok(req)
    }
    /// Only a refused connection means it is safe to launch on this address.
    pub(crate) async fn local_daemon_ready(&self) -> Result<bool> {
        let response = match self
            .request("health")?
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                let error = anyhow::Error::new(error);
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused)
                }) {
                    return Ok(false);
                }
                return Err(error.context("Could not check the local daemon"));
            }
        };
        let health: Value = response
            .error_for_status()?
            .json()
            .await
            .context("Unexpected response from the local daemon address")?;
        ensure!(
            health["status"] == "ok",
            "The local daemon address is occupied by another service"
        );
        match health.get("service") {
            Some(id) => ensure!(
                id == "wakefold",
                "The local daemon address is occupied by another service"
            ),
            None => {
                // Earlier Wakefold versions returned just the status. Confirm their chat API.
                let _: Vec<SavedChat> = self
                    .request("chats")?
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await
                    .context("The local address did not answer the Wakefold chat API")?;
            }
        }
        Ok(true)
    }
    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Ok(self
            .request(path)?
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
    pub async fn download_revision(
        &self,
        chat: Uuid,
        file: &ArtifactRevision,
        destination: &Path,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt as _;
        let mut response = self
            .request(&format!(
                "artifacts/{}/revisions/{}?chat_id={chat}",
                file.artifact_id, file.revision
            ))?
            .timeout(Duration::from_secs(600))
            .send()
            .await?
            .error_for_status()?;
        let temp = tempfile::NamedTempFile::new_in(
            destination.parent().context("Missing destination folder")?,
        )?;
        let mut output = tokio::fs::File::from_std(temp.reopen()?);
        let mut received = 0u64;
        while let Some(chunk) = response.chunk().await? {
            received += chunk.len() as u64;
            ensure!(
                received <= file.size_bytes,
                "Download exceeds the published size"
            );
            output.write_all(&chunk).await?;
        }
        ensure!(received == file.size_bytes, "Incomplete artifact download");
        output.flush().await?;
        output.sync_all().await?;
        drop(output);
        temp.persist(destination)?;
        Ok(())
    }
    async fn post(&self, url: Url, body: Value) -> Result<reqwest::Response> {
        let mut request = self
            .http
            .post(url)
            .timeout(Duration::from_secs(35))
            .json(&body);
        if let Some(auth) = &self.auth {
            request = request.header(AUTHORIZATION, auth);
        }
        let response = request.send().await?;
        if let Err(error) = response.error_for_status_ref() {
            let message = response
                .json::<Value>()
                .await
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_owned));
            return Err(match message {
                Some(msg) => anyhow::Error::new(error).context(msg),
                None => error.into(),
            });
        }
        Ok(response)
    }
    pub async fn send_prompt(&self, chat: Uuid, key: Uuid, prompt: &str) -> Result<i64> {
        Ok(self
            .post(
                self.url.join("runs")?,
                json!({"chat_id":chat,"run_key":key,"prompt":prompt}),
            )
            .await?
            .json::<AcceptedRun>()
            .await?
            .run_id)
    }
    pub async fn decide_approval(&self, id: &str, approved: bool) -> Result<()> {
        let mut url = self.url.join("approvals")?;
        url.path_segments_mut()
            .expect("HTTP URL supports path segments")
            .push(id);
        self.post(url, json!({"approved":approved})).await?;
        Ok(())
    }
    pub async fn chats(&self) -> Result<Vec<SavedChat>> {
        let mut path = "chats".to_string();
        let mut all = Vec::new();
        loop {
            let page: Vec<SavedChat> = self.get(&path).await?;
            let Some(last) = page.last() else { break };
            ensure!(
                all.last()
                    .is_none_or(|c: &SavedChat| page[0].root_agent_id < c.root_agent_id),
                "Invalid chat list cursor"
            );
            path = format!("chats?before={}", last.root_agent_id);
            let full = page.len() == 128;
            all.extend(page);
            if !full {
                break;
            }
        }
        Ok(all)
    }
    async fn snapshot(&self, chat: Option<Uuid>) -> Result<Update> {
        let (chats, approvals, (runs, artifacts)) =
            tokio::try_join!(self.chats(), self.get("approvals"), async {
                match chat {
                    Some(id) => {
                        let runs = format!("chats/{id}/runs");
                        let versions = format!("chats/{id}/artifact-revisions");
                        tokio::try_join!(self.get(&runs), async {
                            let response = self.request(&versions)?.send().await?;
                            // Older daemons still support chatting.
                            if response.status() == reqwest::StatusCode::NOT_FOUND {
                                return Ok(Vec::new());
                            }
                            Ok(response.error_for_status()?.json().await?)
                        })
                    }
                    None => Ok((vec![], vec![])),
                }
            })?;
        Ok(Update::Snapshot {
            chats,
            approvals,
            runs,
            artifacts,
        })
    }
    async fn socket(&self, run: i64, after: u64) -> Result<Socket> {
        let mut url = self.url.join(&format!("runs/{run}/events?after={after}"))?;
        url.set_scheme(if self.tls.is_some() { "wss" } else { "ws" })
            .map_err(|_| anyhow::anyhow!("Invalid WebSocket URL"))?;
        let mut request = url.as_str().into_client_request()?;
        if let Some(auth) = &self.auth {
            request.headers_mut().insert(AUTHORIZATION, auth.clone());
        }
        let connector = self.tls.clone().map(Connector::Rustls);
        Ok(time::timeout(
            Duration::from_secs(10),
            connect_async_tls_with_config(request, None, false, connector),
        )
        .await??
        .0)
    }
    /// A watcher belongs to one server and selection. Reconnecting retains its applied cursor.
    pub async fn watch(&self, chat: Option<Uuid>, run: Option<i64>, updates: mpsc::Sender<Update>) {
        let mut after = 0;
        let mut complete = false;
        loop {
            let result = self
                .follow(chat, run, &mut after, &mut complete, &updates)
                .await;
            let Err(error) = result else { return };
            if updates.is_closed() {
                return;
            }
            if rejected(&error) {
                let _ = updates
                    .send(Update::Attention(
                        "Access denied. Check the server’s access token.".into(),
                    ))
                    .await;
                return;
            }
            if updates
                .send(Update::Reconnecting(format!("{error:#}")))
                .await
                .is_err()
            {
                return;
            }
            time::sleep(Duration::from_secs(2)).await;
        }
    }
    async fn follow(
        &self,
        chat: Option<Uuid>,
        run: Option<i64>,
        after: &mut u64,
        complete: &mut bool,
        updates: &mpsc::Sender<Update>,
    ) -> Result<()> {
        let snapshot = self.snapshot(chat).await?;
        let mut socket = match run {
            Some(id)
                if !*complete
                    && matches!(&snapshot, Update::Snapshot { runs, .. } if runs.iter().any(|r| r.run_id == id)) =>
            {
                Some(self.socket(id, *after).await?)
            }
            _ => None,
        };
        updates.send(snapshot).await?;
        let mut refresh = time::interval_at(
            Instant::now() + Duration::from_secs(5),
            Duration::from_secs(5),
        );
        refresh.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        let mut last_frame = Instant::now();
        loop {
            tokio::select! {
                _ = updates.closed() => return Ok(()),
                _ = refresh.tick() => {
                    if let Some(ws) = socket.as_mut() {
                        ensure!(last_frame.elapsed() < Duration::from_secs(25), "The daemon stopped responding.");
                        ws.send(Message::Ping(Default::default())).await?;
                    }
                    updates.send(self.snapshot(chat).await?).await?;
                }
                message = async { socket.as_mut().expect("socket branch is enabled").next().await }, if socket.is_some() => {
                    last_frame = Instant::now();
                    match message.transpose()? {
                        Some(Message::Text(text)) => {
                            let event: Event = serde_json::from_str(&text)?;
                            ensure!(Some(event.run_id) == run, "Unexpected run in event stream");
                            if event.sequence <= *after {
                                continue;
                            }
                            let sequence = event.sequence;
                            let terminal = matches!(event.event.kind.as_str(), "run_finished" | "run_failed");
                            updates.send(Update::Event(event)).await?;
                            *after = sequence;
                            *complete |= terminal;
                        }
                        Some(Message::Close(_)) | None if *complete => {
                            socket = None;
                            updates.send(self.snapshot(chat).await?).await?;
                        }
                        Some(Message::Close(_)) | None => bail!("The event stream disconnected."),
                        _ => {}
                    }
                }
            }
        }
    }
}
fn rejected(error: &anyhow::Error) -> bool {
    let status = error
        .downcast_ref::<reqwest::Error>()
        .and_then(|e| e.status())
        .or_else(|| match error.downcast_ref::<tungstenite::Error>() {
            Some(tungstenite::Error::Http(r)) => Some(r.status()),
            _ => None,
        });
    status.is_some_and(|s| matches!(s.as_u16(), 401 | 403))
}

#[cfg(test)]
pub(crate) mod tests;
