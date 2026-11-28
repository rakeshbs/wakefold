//! Three successive desktop sessions connect to a separately restarted TLS daemon.
use super::*;
use anyhow::{Context as _, ensure};
use gpui::{KeyBinding, TestAppContext, VisualTestContext, size};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const FIRST: &str = "Find the notebook price and record an approved review.";
const SECOND: &str = "What was the price?";
const ANSWER: &str = "The notebook costs 350 cents and its review was recorded.";
const DRIVER: &str = "servers::workflow_test::chat_window";
const TOKEN: &str = "desktop-chat-restart-fixture-token-7.2";

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn until_file(path: &Path, process: &mut Process) -> Result<String> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(40) {
        if let Ok(text) = fs::read_to_string(path)
            && !text.is_empty()
        {
            return Ok(text);
        }
        ensure!(
            process.0.try_wait()?.is_none(),
            "process exited before {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(30));
    }
    anyhow::bail!("timed out waiting for {}", path.display())
}
fn daemon_for(dir: &Path, test: &str) -> Result<Process> {
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?;
    Ok(Process(
        Command::new(std::env::var_os("WAKEFOLD_TEST_DAEMON").context("Run remote-macos.sh")?)
            .args(["--exact", test, "--nocapture"])
            .env("WAKEFOLD_DAEMON_TEST_CONFIG", dir.join("daemon.json"))
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()?,
    ))
}
fn window(dir: &Path, phase: &str) -> Result<Process> {
    Ok(Process(
        Command::new(std::env::current_exe()?)
            .args(["--exact", DRIVER, "--ignored", "--nocapture"])
            .env("WAKEFOLD_CHAT_TEST_DIR", dir)
            .env("WAKEFOLD_CHAT_TEST_PHASE", phase)
            .spawn()?,
    ))
}
#[test]
#[ignore = "runs desktop and daemon processes with TLS; use crates/desktop/tests/remote-macos.sh"]
fn reopen_chat_and_restart_daemon() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let d = dir.path();
    crate::connection::tests::certificates(d)?;
    fs::write(d.join("token"), TOKEN)?;
    let mut settings = json!({"state_directory":"state","bind":"127.0.0.1:0",
        "remote":{"certificate_chain":"leaf.pem","private_key":"server.key","token_file":"token"},
        "defaults":{"model":"scripted-workflow-provider","max_model_turns":15,"token_budget":1000,
                    "tool_grants":{"orders.review":"require_approval"}}});
    fs::write(d.join("daemon.json"), serde_json::to_vec(&settings)?)?;
    let mut first_daemon = daemon_for(d, "tests::http_workflow_survives_daemon_restart")?;
    let address = until_file(&d.join("listen"), &mut first_daemon)?;
    let port = address.parse::<std::net::SocketAddr>()?.port();
    let mut profiles = Profiles::default();
    profiles.remember(Profile::remote(
        "Mac fixture",
        &format!("https://localhost:{port}"),
    )?);
    profiles.save(&d.join("servers.json"))?;
    ensure!(
        window(d, "start")?.0.wait()?.success(),
        "first desktop session failed"
    );
    let mut reopened = window(d, "reopen")?;
    until_file(&d.join("restored"), &mut reopened)?;
    drop(first_daemon);
    until_file(&d.join("disconnected"), &mut reopened)?;
    settings["bind"] = json!(address);
    fs::write(d.join("daemon.json"), serde_json::to_vec(&settings)?)?;
    fs::remove_file(d.join("listen"))?;
    let mut restarted = daemon_for(d, "tests::http_workflow_survives_daemon_restart")?;
    until_file(&d.join("listen"), &mut restarted)?;
    ensure!(
        reopened.0.wait()?.success(),
        "reopened desktop session failed"
    );
    ensure!(
        window(d, "finished")?.0.wait()?.success(),
        "final history verification failed"
    );
    Ok(())
}

fn wait(
    view: &Entity<Servers>,
    cx: &mut VisualTestContext,
    description: &str,
    predicate: impl Fn(&Servers) -> bool,
) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        if cx.update(|_, app| predicate(view.read(app))) {
            return;
        }
        if start.elapsed() > Duration::from_secs(25) {
            cx.update(|_, app| {
                let s = view.read(app);
                panic!(
                    "Timed out waiting for {description}: {}, {:?}, {} runs, error {:?}",
                    s.status.label(),
                    s.chat,
                    s.runs.len(),
                    s.config_error
                );
            });
        }
        thread::sleep(Duration::from_millis(15));
    }
}
fn wait_control(cx: &mut VisualTestContext, selector: &'static str) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        if cx.debug_bounds(selector).is_some() {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(25),
            "Missing {selector}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}
fn click(cx: &mut VisualTestContext, selector: &str) {
    // GPUI's test frame takes a static selector. Each process clicks only a handful.
    let b = cx
        .debug_bounds(selector.to_owned().leak())
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(b.center(), Default::default());
    cx.run_until_parked();
}
fn send_in_new_chat(view: &Entity<Servers>, cx: &mut VisualTestContext, prompt: &str) {
    click(cx, "new-chat");
    wait(view, cx, "composer", Servers::can_send);
    click(cx, "composer");
    cx.simulate_input(prompt);
    cx.simulate_keystrokes("enter");
}
fn ids(s: &Servers) -> Value {
    json!({"chat":s.chat,"tree":s.runs.iter().take(3).map(|r|(r.agent_id,r.run_id,r.spawned_by_run_id)).collect::<Vec<_>>()})
}

#[gpui::test]
#[ignore = "child process of reopen_chat_and_restart_daemon"]
fn chat_window(cx: &mut TestAppContext) {
    let d = PathBuf::from(std::env::var_os("WAKEFOLD_CHAT_TEST_DIR").unwrap());
    let phase = std::env::var("WAKEFOLD_CHAT_TEST_PHASE").unwrap();
    cx.update(|a| {
        crate::input::bind(a);
        a.bind_keys([KeyBinding::new(
            "enter",
            crate::SendMessage,
            Some("TextInput"),
        )]);
    });
    let (v, cx) = cx.add_window_view(|_, cx| {
        let mut s =
            Servers::with_settings(Runtime::new().unwrap(), Some(d.join("servers.json")), cx);
        // GPUI's test platform has no Keychain. Use the same TLS client with a fixture credential.
        s.load = None;
        s.client = Some(Daemon::new(&s.active, Some(TOKEN), Some(&d.join("ca.pem"))).unwrap());
        s.watch(cx);
        s
    });
    cx.simulate_resize(size(px(1360.), px(900.)));
    wait(&v, cx, "server connection", |s| {
        matches!(s.status, Status::Connected)
    });
    if phase == "start" {
        send_in_new_chat(&v, cx, FIRST);
        wait(&v, cx, "two delegated children and an approval", |s| {
            s.runs.len() == 3
                && s.approvals.len() == 1
                && s.runs
                    .iter()
                    .any(|r| r.outcome.as_deref() == Some("350 cents"))
        });
        let child = cx.update(|_, app| {
            v.read(app)
                .runs
                .iter()
                .find(|r| r.task == "Read the price.")
                .unwrap()
                .run_id
        });
        click(cx, &format!("child-{child}"));
        assert!(cx.debug_bounds("child-panel").is_some());
        cx.update(|_, app| {
            let s = v.read(app);
            assert_eq!(s.run, Some(child));
            assert_eq!(s.runs[1].outcome.as_deref(), Some("350 cents"));
            fs::write(d.join("ids"), ids(s).to_string()).unwrap();
        });
        return;
    }
    let saved: Value = serde_json::from_slice(&fs::read(d.join("ids")).unwrap()).unwrap();
    wait(&v, cx, "saved chat", |s| s.runs.len() >= 3);
    cx.update(|_, app| assert_eq!(ids(v.read(app)), saved));
    if phase == "reopen" {
        assert!(cx.debug_bounds("child-panel").is_some());
        fs::write(d.join("restored"), "ok").unwrap();
        wait(&v, cx, "disconnect", |s| {
            matches!(s.status, Status::Reconnecting(_))
        });
        fs::write(d.join("disconnected"), "ok").unwrap();
        wait(&v, cx, "pending approval after restart", |s| {
            matches!(s.status, Status::Connected) && s.approvals.len() == 1
        });
        assert!(cx.debug_bounds("child-panel").is_some());
        let wait_id = cx.update(|_, app| {
            let s = v.read(app);
            assert_eq!(ids(s), saved);
            s.approvals[0].wait["id"].as_str().unwrap().to_owned()
        });
        click(cx, &format!("approve-{wait_id}"));
        wait(&v, cx, "root and both children to finish", |s| {
            s.runs.iter().all(Run::finished) && s.approvals.is_empty()
        });
        click(cx, "close-child");
        click(cx, "composer");
        cx.simulate_input(SECOND);
        click(cx, "send");
    }
    wait(&v, cx, "follow-up on the same root", |s| {
        s.runs.len() == 4 && s.runs[3].outcome.as_deref() == Some("350 cents.")
    });
    cx.update(|_, app| {
        let s = v.read(app);
        assert_eq!(ids(s), saved);
        let root = &s.runs[0];
        assert_eq!(root.outcome.as_deref(), Some(ANSWER));
        assert_eq!(s.runs[3].agent_id, root.agent_id);
        assert_ne!(s.runs[3].run_id, root.run_id);
        assert_eq!(s.runs[3].task, SECOND);
        let children: Vec<_> = s
            .runs
            .iter()
            .filter(|r| r.spawned_by_run_id == Some(root.run_id))
            .collect();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].outcome.as_deref(), Some("350 cents"));
        assert_eq!(children[1].outcome.as_deref(), Some("reviewed"));
        assert!(s.profiles.pending_requests.is_empty());
        assert!(s.approvals.is_empty());
        if phase == "finished" {
            let client = s.client.as_ref().unwrap();
            s.runtime.block_on(async {
                let follow_up = &s.runs[3];
                let retry = client
                    .send_prompt(
                        s.chat.unwrap(),
                        Uuid::parse_str(&follow_up.run_key).unwrap(),
                        SECOND,
                    )
                    .await
                    .unwrap();
                assert_eq!(retry, follow_up.run_id);
                let messages: Vec<Value> = client
                    .get(&format!("runs/{}/messages", root.run_id))
                    .await
                    .unwrap();
                assert_eq!(messages.len(), children.len());
                for child in children {
                    let matching: Vec<_> = messages
                        .iter()
                        .filter(|m| m["from_agent_id"] == child.agent_id)
                        .collect();
                    assert_eq!(matching.len(), 1);
                    let message = matching[0];
                    assert_eq!(message["message_kind"], "child_outcome");
                    assert_eq!(
                        message["payload"]["answer"].as_str(),
                        child.outcome.as_deref()
                    );
                    assert!(message["received_event_sequence"].is_number());
                }
            });
        }
    });
    if phase == "finished" {
        send_in_new_chat(&v, cx, "Record the review.");
        wait(&v, cx, "new chat approval", |s| {
            s.runs.len() == 1 && s.approvals.len() == 1
        });
        let wait_id = cx.update(|_, app| {
            v.read(app).approvals[0].wait["id"]
                .as_str()
                .unwrap()
                .to_owned()
        });
        click(cx, &format!("decline-{wait_id}"));
        wait(&v, cx, "declined review", |s| {
            s.approvals.is_empty() && s.runs[0].outcome.as_deref() == Some("Review declined.")
        });
    }
}

#[gpui::test]
#[ignore = "uses a separate TLS daemon; run remote-macos.sh"]
fn manage_mcp_and_download_revisions(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    crate::connection::tests::certificates(d).unwrap();
    fs::write(d.join("token"), TOKEN).unwrap();
    fs::write(
        d.join("catalog_mcp.py"),
        include_str!("../../../daemon/tests/fixtures/catalog_mcp.py"),
    )
    .unwrap();
    let config = json!({"state_directory":"state","allow_mcp_configuration":true,"bind":"127.0.0.1:0",
        "remote":{"certificate_chain":"leaf.pem","private_key":"server.key","token_file":"token"},
        "defaults":{"model":"scripted","max_model_turns":20,"token_budget":300}});
    fs::write(d.join("daemon.json"), config.to_string()).unwrap();
    let fixture = "tests::artifacts::artifact_publication_survives_daemon_crash";
    let mut process = daemon_for(d, fixture).unwrap();
    let address = until_file(&d.join("listen"), &mut process).unwrap();
    let port = address.parse::<std::net::SocketAddr>().unwrap().port();
    let profile = Profile::remote("Mac fixture", &format!("https://localhost:{port}")).unwrap();
    let mut profiles = Profiles::default();
    profiles.remember(profile.clone());
    profiles.save(&d.join("servers.json")).unwrap();
    cx.update(|app| {
        crate::input::bind(app);
        app.bind_keys([KeyBinding::new(
            "enter",
            crate::SendMessage,
            Some("TextInput"),
        )]);
    });
    let (view, cx) = cx.add_window_view(|_, cx| {
        let mut s =
            Servers::with_settings(Runtime::new().unwrap(), Some(d.join("servers.json")), cx);
        s.load = None;
        s.client = Some(Daemon::new(&profile, Some(TOKEN), Some(&d.join("ca.pem"))).unwrap());
        s.watch(cx);
        s
    });
    cx.simulate_resize(size(px(1360.), px(1450.)));
    wait(&view, cx, "connected", |s| {
        matches!(s.status, Status::Connected)
    });
    click(cx, "mcp-servers");
    wait_control(cx, "new-mcp");
    click(cx, "new-mcp");
    for (selector, value) in [("mcp-name", "catalog"), ("mcp-command", "python3")] {
        click(cx, selector);
        cx.simulate_input(value);
    }
    click(cx, "add-arg");
    click(cx, "mcp-arg-0");
    cx.simulate_input("catalog_mcp.py");
    click(cx, "discover-mcp");
    wait_control(cx, "expose-lookup.price");
    assert!(cx.debug_bounds("expose-private_notes").is_some());
    click(cx, "expose-lookup.price");
    click(cx, "grant-allow-0");
    click(cx, "save-mcp");
    wait_control(cx, "edit-catalog");
    let saved: Value = serde_json::from_slice(&fs::read(d.join("daemon.json")).unwrap()).unwrap();
    assert_eq!(
        saved["defaults"]["tool_grants"]["catalog.lookup_price"],
        "allow"
    );
    click(cx, "edit-catalog");
    wait(&view, cx, "edit saved catalog", |s| {
        s.mcp.as_ref().is_some_and(|m| m.editor.is_some())
    });
    click(cx, "grant-approval-0");
    click(cx, "save-mcp");
    wait_control(cx, "edit-catalog");
    wait(&view, cx, "saved approval grant", |s| {
        s.mcp.as_ref().is_some_and(|m| m.editor.is_none())
            && serde_json::from_slice::<Value>(&fs::read(d.join("daemon.json")).unwrap()).unwrap()["defaults"]
                ["tool_grants"]["catalog.lookup_price"]
                == "require_approval"
    });
    // Keep the same ephemeral TLS address across restarts.
    let mut saved: Value =
        serde_json::from_slice(&fs::read(d.join("daemon.json")).unwrap()).unwrap();
    assert_eq!(
        saved["defaults"]["tool_grants"]["catalog.lookup_price"],
        "require_approval"
    );
    saved["bind"] = json!(address);
    fs::write(d.join("daemon.json"), saved.to_string()).unwrap();
    let restart = |process: Process, cx: &mut VisualTestContext| {
        drop(process);
        wait(&view, cx, "daemon disconnect", |s| {
            matches!(s.status, Status::Reconnecting(_))
        });
        fs::remove_file(d.join("listen")).unwrap();
        let mut next = daemon_for(d, fixture).unwrap();
        until_file(&d.join("listen"), &mut next).unwrap();
        wait(&view, cx, "daemon reconnect", |s| {
            matches!(s.status, Status::Connected)
        });
        next
    };
    let configured = restart(process, cx);
    wait_control(cx, "schema-catalog-lookup.price");
    click(cx, "schema-catalog-lookup.price");
    send_in_new_chat(&view, cx, "Publish two revisions of the inventory report.");
    wait(&view, cx, "child MCP approval", |s| {
        s.runs.len() == 2 && s.approvals.len() == 1
    });
    let (wait_id, child_id) = cx.update(|_, app| {
        let s = view.read(app);
        let approval = &s.approvals[0];
        assert_eq!(
            approval.wait["on"]["ToolApproval"]["tool_name"],
            "catalog.lookup_price"
        );
        assert_eq!(s.runs[1].agent_id, approval.agent_id);
        (
            approval.wait["id"].as_str().unwrap().to_owned(),
            approval.agent_id,
        )
    });
    let running = restart(configured, cx);
    cx.update(|_, app| assert_eq!(view.read(app).approvals[0].agent_id, child_id));
    click(cx, &format!("approve-{wait_id}"));
    wait(&view, cx, "two published revisions", |s| {
        s.artifacts.revisions.len() == 2 && s.runs.iter().all(Run::finished)
    });
    assert!(cx.debug_bounds("Download-1-1").is_some());
    click(cx, "chat-artifacts");
    let _restarted = restart(running, cx);
    assert_eq!(
        fs::read_to_string(d.join("catalog-calls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    for (revision, quantity) in [(1, 4), (2, 6)] {
        let dest = d.join(format!("saved-{revision}.csv"));
        click(cx, &format!("Download-1-{revision}"));
        assert!(cx.did_prompt_for_new_path());
        cx.simulate_new_path_selection(|_| Some(dest.clone()));
        wait(&view, cx, "download", |_| dest.exists());
        assert_eq!(
            fs::read_to_string(dest).unwrap(),
            format!("item,quantity\nnotebook,{quantity}\n")
        );
    }
}

#[gpui::test]
#[ignore = "uses the real local daemon; run local-daemon-macos.py"]
fn select_local_computer(cx: &mut TestAppContext) {
    let directory = PathBuf::from(
        std::env::var_os("WAKEFOLD_CONFIG_DIR")
            .expect("the macOS test supplies a temporary profile"),
    );
    let settings = directory.join("servers.json");
    assert!(!settings.exists() && !directory.join("daemon.json").exists());
    let mut profiles = Profiles::default();
    let remote = Profile::remote("Remote fixture", "https://localhost:9999").unwrap();
    profiles.remember(remote.clone());
    profiles.save(&settings).unwrap();
    let (view, cx) = cx.add_window_view(|_, cx| Servers::new(Runtime::new().unwrap(), cx));
    cx.simulate_resize(size(px(1180.), px(745.)));
    wait(&view, cx, "saved remote selection", |s| {
        s.active.url == remote.url && matches!(s.status, Status::Attention(_))
    });
    assert!(!settings.with_file_name("daemon-start.lock").exists());
    click(cx, "profile-0");
    wait(&view, cx, "local daemon", |s| {
        s.active.url == LOCAL && matches!(s.status, Status::Connected)
    });
    assert_eq!(
        Profiles::load(&settings).unwrap().selected.as_deref(),
        Some(LOCAL)
    );
}
