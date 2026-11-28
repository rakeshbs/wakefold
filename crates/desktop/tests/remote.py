"""Run the GPUI desktop workflows against a separate daemon on localhost."""

import json
import os
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[3]
TESTS = (
    "connection::tests::reconnect_to_remote_daemon",
    "servers::workflow_test::reopen_chat_and_restart_daemon",
    "servers::workflow_test::manage_mcp_and_download_revisions",
)


def main():
    build = subprocess.run(
        ["cargo", "test", "--locked", "-p", "wakefold-daemon", "--bin", "wakefold",
         "--no-run", "--message-format=json-render-diagnostics"],
        cwd=ROOT, stdout=subprocess.PIPE, encoding="utf-8", check=True,
    )
    daemon, = (
        record["executable"]
        for record in map(json.loads, build.stdout.splitlines())
        if record["reason"] == "compiler-artifact" and record["target"]["name"] == "wakefold"
        and record["profile"]["test"] and record["executable"]
    )
    env = {**os.environ, "WAKEFOLD_TEST_DAEMON": daemon, "WAKEFOLD_TEST_PYTHON": sys.executable}
    cargo = ["cargo", "test", "--locked", "-p", "wakefold-desktop", "--bin", "wakefold-desktop"]
    listing = subprocess.check_output([*cargo, "--", "--list", "--ignored"], cwd=ROOT, encoding="utf-8")
    for test in TESTS:
        if f"{test}: test" not in listing.splitlines():
            raise SystemExit(f"Missing desktop acceptance test: {test}")
        subprocess.run([*cargo, "--", "--exact", test, "--ignored", "--nocapture"],
                       cwd=ROOT, env=env, check=True)


if __name__ == "__main__":
    main()
