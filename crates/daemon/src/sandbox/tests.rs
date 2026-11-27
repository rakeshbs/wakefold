use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use wakefold::WorkspaceStore;

use super::{Config, Sandbox};

const PROBE: &str = r#"
import json, os, pathlib, socket, sys
args = json.load(sys.stdin)
agents = pathlib.Path("/workspace/agents")
peer = agents / str(args["peer"]) / "shared.txt"
hidden = agents / str(args["hidden"]) / "private.txt"
other = agents / str(args["other"]) / "private.txt"
pathlib.Path(args["output"]).write_text("written by sandbox")
try:
    peer.write_text("changed")
    can_write = True
except OSError:
    can_write = False
try:
    peer.unlink()
    can_unlink = True
except OSError:
    can_unlink = False
print(json.dumps({
    "uid": os.getuid(),
    "cwd": os.getcwd(),
    "peer": peer.read_text(),
    "can_write": can_write,
    "can_unlink": can_unlink,
    "hidden": hidden.exists(),
    "other": other.exists(),
    "host_database": pathlib.Path(args["database"]).exists(),
    "host_credentials": "OPENAI_API_KEY" in os.environ,
    "interfaces": [name for _, name in socket.if_nameindex()],
}))
"#;

fn python(source: &str) -> Vec<String> {
    ["/usr/bin/python3", "-I", "-c", source]
        .map(str::to_owned)
        .into()
}

#[tokio::test]
#[ignore = "requires a real Apple container service or Linux Bubblewrap and systemd user session"]
async fn workspace_permissions_and_limits_survive_reuse() {
    let state = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let caller = store.open_run_workspace("chat", "caller").unwrap();
    let peer = store.open_run_workspace("chat", "shared").unwrap();
    let hidden = store.open_run_workspace("chat", "unshared").unwrap();
    let other = store.open_run_workspace("other", "outside").unwrap();
    for (workspace, name) in [
        (&peer, "shared.txt"),
        (&hidden, "private.txt"),
        (&other, "private.txt"),
    ] {
        fs::write(
            workspace
                .chat_root()
                .join(workspace.agent_relative_directory())
                .join(name),
            "original",
        )
        .unwrap();
    }
    let backend = if cfg!(target_os = "macos") {
        json!({
            "kind": "apple_container",
            "image": std::env::var("WAKEFOLD_SANDBOX_TEST_IMAGE").expect("supply an Apple container test image"),
            "executable": std::env::var_os("WAKEFOLD_SANDBOX_TEST_CONTAINER").map_or_else(|| PathBuf::from("container"), PathBuf::from),
        })
    } else {
        json!({"kind": "bubblewrap"})
    };
    let config: Config = serde_json::from_value(json!({
        "backend": backend, "memory_mib": 96, "call_timeout_seconds": 2,
        "idle_seconds": 1,
        "read_peers": {caller.agent_id().to_string(): [peer.agent_id()]},
    }))
    .unwrap();
    let sandbox = Sandbox::new(config, state.path()).await.unwrap();
    let probe_input = |output: &str| {
        json!({
            "peer": peer.agent_id(),
            "hidden": hidden.agent_id(),
            "other": other.agent_id(),
            "database": state.path().join("wakefold.sqlite3"),
            "output": output,
        })
    };
    let report = sandbox
        .run(&caller, &python(PROBE), &probe_input("first.csv"))
        .await
        .unwrap();
    assert_eq!(
        report["cwd"],
        format!("/workspace/agents/{}", caller.agent_id())
    );
    assert!(report["uid"].as_u64().unwrap() > 0);
    assert_eq!(report["peer"], "original");
    for key in [
        "can_write",
        "can_unlink",
        "hidden",
        "other",
        "host_database",
        "host_credentials",
    ] {
        assert_eq!(report[key], false, "{key}: {report}");
    }
    assert_eq!(report["interfaces"], json!(["lo"]));
    let slot = sandbox.agents.lock().await[&caller.agent_id()].clone();
    let first_pid = slot.lock().await.as_ref().unwrap().child.id();
    let second = sandbox
        .run(&caller, &python(PROBE), &probe_input("second.csv"))
        .await
        .unwrap();
    assert_eq!(second, report);
    assert_eq!(slot.lock().await.as_ref().unwrap().child.id(), first_pid);
    let peer_uid = sandbox
        .run(
            &peer,
            &python("import json,os; print(json.dumps(os.getuid()))"),
            &Value::Null,
        )
        .await
        .unwrap();
    assert_ne!(
        peer_uid, report["uid"],
        "agent processes need separate identities"
    );
    if cfg!(target_os = "macos") {
        assert_eq!(
            sandbox.chats.lock().await.len(),
            1,
            "one VM per conversation"
        );
    }

    let working = caller.chat_root().join(caller.agent_relative_directory());
    for name in ["first.csv", "second.csv"] {
        assert_eq!(
            fs::read_to_string(working.join(name)).unwrap(),
            "written by sandbox"
        );
    }

    let timeout_script = r#"
import subprocess, sys, time
subprocess.Popen([sys.executable, "-I", "-c",
    'import time,pathlib; time.sleep(2.5); pathlib.Path("escaped-timeout").touch()'])
time.sleep(30)
"#;
    let timeout = sandbox
        .run(&caller, &python(timeout_script), &Value::Null)
        .await
        .unwrap_err();
    assert!(timeout.to_string().contains("timed out"), "{timeout}");
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(!working.join("escaped-timeout").exists());

    let cancel_script = r#"
import pathlib, subprocess, sys, time
subprocess.Popen([sys.executable, "-I", "-c",
    'import time,pathlib; time.sleep(1); pathlib.Path("escaped-cancel").touch()'])
pathlib.Path("cancel-started").touch()
time.sleep(30)
"#;
    {
        let argv = python(cancel_script);
        let invocation = sandbox.run(&caller, &argv, &Value::Null);
        tokio::pin!(invocation);
        let started = async {
            while !working.join("cancel-started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            result = &mut invocation => panic!("tool ended before test could cancel it: {result:?}"),
            ready = tokio::time::timeout(Duration::from_secs(8), started) => {
                ready.expect("test must cancel a running tool");
            }
        }
    }
    tokio::time::sleep(Duration::from_millis(1150)).await;
    assert!(!working.join("escaped-cancel").exists());

    // A cgroup OOM can terminate its sandbox supervisor along with the tool.
    // Exercise it in a separate agent so it cannot mask timeout assertions.
    sandbox
        .run(&hidden, &python("print('true')"), &Value::Null)
        .await
        .unwrap();
    let memory = sandbox
        .run(
            &hidden,
            &python(
                "x = bytearray(192 * 1024 * 1024); x[::4096] = b'x' * (192 * 256); print('true')",
            ),
            &Value::Null,
        )
        .await;
    assert!(memory.is_err(), "the command must hit the agent memory cap");

    // Release the test's reference so the idle sweep can retire the session.
    drop(slot);
    let stop = wakefold::CancellationToken::new();
    let wait_for_idle = async {
        let idle = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                if sandbox.agents.lock().await.is_empty() && sandbox.chats.lock().await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await;
        stop.cancel();
        idle.expect("idle sessions and their chat VM must be retired");
    };
    tokio::join!(sandbox.maintain(&stop), wait_for_idle);
    assert!(
        working.join("first.csv").exists(),
        "workspace survives sandbox cleanup"
    );
}
