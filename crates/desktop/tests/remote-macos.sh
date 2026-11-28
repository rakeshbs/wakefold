#!/bin/sh
# Exercise the desktop transport against the daemon with its deterministic mailbox provider.
set -eu
cd "$(dirname "$0")/../../.."
daemon_test=$(cargo test --locked --no-run -p wakefold-daemon --message-format=json |
  python3 -c 'import json, sys
print([record["executable"] for line in sys.stdin if (record := json.loads(line)).get("executable") and record["target"]["name"] == "wakefold"].pop())')
WAKEFOLD_TEST_DAEMON="$daemon_test" cargo test --locked -p wakefold-desktop connection::tests::reconnect_to_remote_daemon -- --exact --ignored --nocapture
