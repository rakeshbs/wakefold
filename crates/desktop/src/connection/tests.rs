use super::*;
use serde_json::json;
use std::{
    fs,
    net::SocketAddr,
    process::{Command, Stdio},
};
use tokio::{net::TcpListener, sync::watch, task::JoinSet};

pub(crate) fn certificates(dir: &Path) -> Result<()> {
    fn openssl(dir: &Path, args: &[&str]) -> Result<()> {
        let output = Command::new("openssl")
            .args(args)
            .current_dir(dir)
            .output()?;
        ensure!(
            output.status.success(),
            "openssl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
    fs::write(
        dir.join("ca.cnf"),
        "[req]\ndistinguished_name=dn\nprompt=no\nx509_extensions=ca\n[dn]\nCN=Wakefold Test CA\n[ca]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\n",
    )?;
    fs::write(
        dir.join("leaf.cnf"),
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n",
    )?;
    openssl(
        dir,
        &[
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-config", "ca.cnf",
            "-keyout", "ca.key", "-out", "ca.pem",
        ],
    )?;
    openssl(
        dir,
        &[
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=localhost",
            "-keyout",
            "server.key",
            "-out",
            "leaf.csr",
        ],
    )?;
    openssl(
        dir,
        &[
            "x509",
            "-req",
            "-in",
            "leaf.csr",
            "-CA",
            "ca.pem",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-days",
            "1",
            "-extfile",
            "leaf.cnf",
            "-out",
            "leaf.pem",
        ],
    )
}

// This forwards encrypted bytes. Pausing it drops existing connections while the daemon keeps running.
async fn network_link(upstream: SocketAddr) -> Result<(SocketAddr, watch::Sender<bool>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (control, mut enabled) = watch::channel(true);
    tokio::spawn(async move {
        let mut links = JoinSet::new();
        loop {
            tokio::select! {
                changed=enabled.changed()=>{
                    if changed.is_err(){break;}
                    if !*enabled.borrow(){links.shutdown().await;}
                }
                Some(_)=links.join_next(),if !links.is_empty()=>{}
                Ok((mut socket,_))=listener.accept()=>{
                    if *enabled.borrow(){links.spawn(async move{
                        if let Ok(mut daemon)=TcpStream::connect(upstream).await{let _=tokio::io::copy_bidirectional(&mut socket,&mut daemon).await;}
                    });}
                }
            }
        }
    });
    Ok((address, control))
}
async fn post(d: &Daemon, path: &str, body: Value) -> Result<reqwest::Response> {
    Ok(d.http
        .post(d.url.join(path)?)
        .header(AUTHORIZATION, d.auth.as_ref().unwrap())
        .json(&body)
        .send()
        .await?
        .error_for_status()?)
}

#[tokio::test]
#[ignore = "launches a daemon process and opens TLS sockets; run crates/desktop/tests/remote-macos.sh"]
async fn reconnect_to_remote_daemon() -> Result<()> {
    time::timeout(Duration::from_secs(55), scenario()).await?
}

async fn scenario() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path();
    certificates(base)?;
    let token = "temporary-test-credential-for-this-process";
    fs::write(base.join("token"), token)?;
    let config = base.join("daemon.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!({
            "state_directory":"state","bind":"127.0.0.1:0",
            "remote":{"certificate_chain":"leaf.pem","private_key":"server.key","token_file":"token"},
            "skills":{"notices":{"description":"Wait for a notice","tools":[{"kind":"timer_wait"},{"kind":"wait_for_message"}]}},
            "defaults":{"model":"scripted-workflow-provider","max_model_turns":6,"token_budget":30,"tool_grants":{"notices.wait":"require_approval"}}
        }))?,
    )?;
    let executable = std::env::var_os("WAKEFOLD_TEST_DAEMON")
        .context("Run crates/desktop/tests/remote-macos.sh")?;
    let mut process = tokio::process::Command::new(executable)
        .kill_on_drop(true)
        .args([
            "--exact",
            "tests::http_messages_survive_daemon_restart",
            "--nocapture",
        ])
        .env("WAKEFOLD_DAEMON_TEST_CONFIG", &config)
        .stdout(Stdio::null())
        .spawn()?;
    let address = loop {
        ensure!(
            process.try_wait()?.is_none(),
            "Daemon exited during startup."
        );
        if let Some(a) = fs::read_to_string(base.join("listen"))
            .ok()
            .and_then(|s| s.parse::<SocketAddr>().ok())
        {
            break a;
        }
        time::sleep(Duration::from_millis(20)).await;
    };
    let ca = base.join("ca.pem");
    let profile = Profile::remote(
        "Mac daemon",
        &format!("https://localhost:{}", address.port()),
    )?;
    let daemon = Daemon::new(&profile, Some(token), Some(&ca))?;
    let empty = post(&daemon, "chats", json!({}))
        .await?
        .json::<Value>()
        .await?;
    assert_eq!(
        daemon.chats().await?[0].chat_id.to_string(),
        empty["chat_id"].as_str().unwrap()
    );
    let chat = Uuid::from_u128(42);
    let root=post(&daemon,"runs",json!({"chat_id":chat,"run_key":"remote-check","prompt":"After approval, receive two client messages."})).await?.json::<Value>().await?;
    let run = root["run_id"].as_i64().unwrap();
    for supplied in [None, Some("wrong")] {
        let mut invalid = daemon.clone();
        invalid.auth = supplied.map(|s| HeaderValue::from_str(&format!("Bearer {s}")).unwrap());
        assert!(rejected(&invalid.chats().await.unwrap_err()));
        assert!(rejected(&invalid.socket(run, 0).await.unwrap_err()));
    }
    let untrusted = Daemon::new(&profile, Some(token), None)?;
    assert!(untrusted.chats().await.is_err() && untrusted.socket(run, 0).await.is_err());
    let wrong_host = Profile::remote(
        "Wrong name",
        &format!("https://127.0.0.1:{}", address.port()),
    )?;
    let mismatch = Daemon::new(&wrong_host, Some(token), Some(&ca))?;
    assert!(mismatch.chats().await.is_err() && mismatch.socket(run, 0).await.is_err());
    let (link, online) = network_link(address).await?;
    let via = Profile::remote(
        "Via test link",
        &format!("https://localhost:{}", link.port()),
    )?;
    let watched = Daemon::new(&via, Some(token), Some(&ca))?;
    let (tx, mut rx) = mpsc::channel(64);
    let watcher = tokio::spawn(async move { watched.watch(Some(chat), Some(run), tx).await });
    let mut seen = Vec::new();
    let wait = loop {
        match rx.recv().await.context("watcher stopped")? {
            Update::Event(e) => {
                let id = (e.event.kind == "wait_started")
                    .then(|| e.event.data["id"].as_str().unwrap().to_owned());
                seen.push(e);
                if let Some(id) = id {
                    break id;
                }
            }
            Update::Attention(e) | Update::Reconnecting(e) => bail!("{e}"),
            _ => {}
        }
    };
    online.send(false)?;
    while !matches!(
        rx.recv().await.context("watcher stopped")?,
        Update::Reconnecting(_)
    ) {}
    let before = seen.last().unwrap().sequence;
    let pending: Vec<Approval> = daemon.get("approvals").await?;
    assert_eq!(pending[0].wait["id"], wait);
    post(
        &daemon,
        &format!("approvals/{wait}"),
        json!({"approved":true}),
    )
    .await?;
    for (key, state) in [
        ("notice-1", "queued before waiting"),
        ("notice-2", "arrived while waiting"),
    ] {
        post(
            &daemon,
            &format!("runs/{run}/messages"),
            json!({"message_key":key,"payload":{"status":state}}),
        )
        .await?;
    }
    let history = loop {
        let events: Vec<Event> = daemon.get(&format!("chats/{chat}/history")).await?;
        if events
            .last()
            .is_some_and(|e| e.event.kind == "run_finished")
        {
            break events;
        }
        if events.last().is_some_and(|e| e.event.kind == "run_failed") {
            bail!("Fixture run failed");
        }
        time::sleep(Duration::from_millis(30)).await;
    };
    online.send(true)?;
    let mut refreshed = false;
    loop {
        match rx.recv().await.context("watcher stopped")? {
            Update::Snapshot {
                runs, approvals, ..
            } => {
                refreshed = runs
                    .iter()
                    .any(|r| r.run_id == run && r.status == "completed")
                    && approvals.is_empty();
            }
            Update::Event(e) => {
                assert!(
                    refreshed,
                    "run tree and approvals must refresh before replay"
                );
                let done = e.event.kind == "run_finished";
                seen.push(e);
                if done {
                    break;
                }
            }
            Update::Attention(e) => bail!("{e}"),
            _ => {}
        }
    }
    assert_eq!(seen, history);
    assert_eq!(seen.last().unwrap().event.data, "Received both messages");
    println!(
        "HTTPS/WSS passed authentication and certificate checks. Dropped after sequence {before}; recovered all {} events and exactly one final result.",
        seen.len()
    );
    watcher.abort();
    process.kill().await?;
    Ok(())
}
