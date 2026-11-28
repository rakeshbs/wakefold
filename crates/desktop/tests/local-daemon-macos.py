"""Exercise local daemon ownership using the native macOS app and real daemon."""
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

LOCAL = "http://127.0.0.1:8700/"
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def api(path, method="GET"):
    request = urllib.request.Request(LOCAL + path, method=method)
    with HTTP.open(request, timeout=2) as response:
        return json.load(response)


def wait_for(description, check, timeout=25):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        result = check()
        if result:
            return result
        time.sleep(0.1)
    raise RuntimeError(f"Timed out waiting for {description}")


def owned_listener(config):
    result = subprocess.run(
        ["/usr/sbin/lsof", "-nP", "-t", "-iTCP:8700", "-sTCP:LISTEN"],
        capture_output=True, text=True, check=False,
    )
    for pid in set(result.stdout.split()):
        process = subprocess.run(
            ["/bin/ps", "-p", pid, "-o", "command="], capture_output=True, text=True,
        )
        if process.stdout.strip().endswith(f"/wakefold serve {config}"):
            return int(pid)
    return None


def connected(settings, app):
    if app.poll() is not None:
        raise RuntimeError(f"Desktop exited early with status {app.returncode}")
    if settings.exists():
        return json.loads(settings.read_text()).get("selected") == LOCAL
    return False


def quit_app(app):
    # Ask Cocoa to deliver a normal quit to this specific application instance.
    subprocess.run([
        "/usr/bin/osascript", "-l", "JavaScript", "-e",
        f"ObjC.import('AppKit'); $.NSRunningApplication.runningApplicationWithProcessIdentifier({app.pid}).terminate",
    ], check=True, stdout=subprocess.DEVNULL, timeout=10)
    assert app.wait(timeout=10) == 0


def exercise(desktop, daemon, root):
    config = root / "daemon.json"
    settings = root / "servers.json"
    env = dict(os.environ, WAKEFOLD_CONFIG_DIR=str(root), OPENAI_API_KEY="local-test-unused")
    apps = []
    manual = None
    with (root / "desktop.log").open("wb") as log:
        def open_app(directory, key=True):
            environment = dict(env, WAKEFOLD_CONFIG_DIR=str(directory))
            if not key:
                environment.pop("OPENAI_API_KEY")
            app = subprocess.Popen([desktop], env=environment, stdout=log, stderr=log)
            apps.append(app)
            return app

        try:
            # Two independent app instances race the first launch.
            first = open_app(root)
            second = open_app(root)
            pid = wait_for("desktop-started daemon", lambda: owned_listener(config))
            wait_for("local selection", lambda: connected(settings, first))
            assert api("health")["service"] == "wakefold"
            chat = api("chats", "POST")["chat_id"]
            for app in (first, second):
                quit_app(app)
            assert owned_listener(config) == pid
            assert any(c["chat_id"] == chat for c in api("chats"))
            startup_log = (root / "daemon.log").read_text()
            assert startup_log.count("Wakefold listening on") == 1 and "Error:" not in startup_log

            # A fresh client with no configuration or key must reuse the live daemon.
            fresh = root / "fresh-client"
            reopened = open_app(fresh, key=False)
            wait_for("reopened local connection", lambda: connected(fresh / "servers.json", reopened))
            assert owned_listener(config) == pid
            assert not (fresh / "daemon.json").exists()
            quit_app(reopened)
            assert api("health")["status"] == "ok"
            print(f"Cold start, two desktop quits and a fresh launch reused daemon PID {pid}.", flush=True)

            os.kill(pid, signal.SIGINT)
            wait_for("test daemon shutdown", lambda: owned_listener(config) is None)
            manual = subprocess.Popen([daemon, "serve", config], env=env, stdout=log, stderr=log)
            wait_for("manually started daemon", lambda: owned_listener(config) == manual.pid)
            another = root / "manual-client"
            attached = open_app(another, key=False)
            wait_for("manual daemon reuse", lambda: connected(another / "servers.json", attached))
            quit_app(attached)
            assert manual.poll() is None and owned_listener(config) == manual.pid
            assert any(c["chat_id"] == chat for c in api("chats"))
            assert not (another / "daemon-start.lock").exists()
            print(f"The app also reused independently started daemon PID {manual.pid} and the chat survived.", flush=True)
        finally:
            for app in apps:
                if app.poll() is None:
                    app.kill()
                    app.wait()
            if manual and manual.poll() is None:
                manual.send_signal(signal.SIGINT)
                manual.wait(timeout=10)
            pid = owned_listener(config)
            if pid:
                os.kill(pid, signal.SIGINT)
                wait_for("fixture cleanup", lambda: owned_listener(config) is None)


def executables(args, repo):
    output = subprocess.run(
        ["cargo", *args, "--locked", "--message-format=json"], cwd=repo, check=True,
        stdout=subprocess.PIPE, text=True,
    )
    binaries = {}
    for line in output.stdout.splitlines():
        record = json.loads(line)
        if record.get("executable"):
            binaries[record["target"]["name"]] = record["executable"]
    return binaries


def sidebar_selection(repo, daemon, root):
    driver = executables(["test", "-p", "wakefold-desktop", "--no-run"], repo)["wakefold-desktop"]
    # Exercise the same sibling-executable layout as the installed app.
    shutil.copy2(driver, root / "sidebar-test")
    (root / "wakefold").symlink_to(daemon)
    config = root / "daemon.json"
    try:
        subprocess.run([
            root / "sidebar-test", "--exact", "--ignored",
            "servers::workflow_test::select_local_computer",
        ], env=dict(os.environ, WAKEFOLD_CONFIG_DIR=str(root), OPENAI_API_KEY="local-test-unused"),
            check=True, timeout=45)
        assert api("health")["service"] == "wakefold" and owned_listener(config)
        print("Switching from a saved remote profile to This Mac started the local daemon.", flush=True)
    finally:
        pid = owned_listener(config)
        if pid:
            os.kill(pid, signal.SIGINT)
            wait_for("sidebar fixture cleanup", lambda: owned_listener(config) is None)


def main():
    if sys.platform != "darwin":
        raise SystemExit("This test requires macOS.")
    # An occupied port belongs to the user. Only stop our temporary fixture.
    with socket.socket() as probe:
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        probe.bind(("127.0.0.1", 8700))
    repo = Path(__file__).resolve().parents[3]
    binaries = executables(["build", "-p", "wakefold-daemon", "-p", "wakefold-desktop"], repo)
    with tempfile.TemporaryDirectory(prefix="wakefold-local-") as directory:
        exercise(binaries["wakefold-desktop"], binaries["wakefold"], Path(directory))
    with tempfile.TemporaryDirectory(prefix="wakefold-select-") as directory:
        sidebar_selection(repo, binaries["wakefold"], Path(directory))


if __name__ == "__main__":
    main()
