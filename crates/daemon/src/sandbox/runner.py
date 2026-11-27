"""Trusted supervisor for a single agent's Linux sandbox."""
import ctypes
import errno
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import tempfile
import time

MAX_OUTPUT = 1024 * 1024
SCRIPT = "/wakefold/runner.py"
CGROUP_ROOT = Path("/sys/fs/cgroup")
AGENT_CONTROLLERS = {"memory", "pids"}


def exec_sandbox(profile):
    """Build the mount namespace before accepting any model input."""
    if profile.get("guest_cgroup"):
        enter_cgroup(profile)

    uid = str(10000 + profile["agent"])
    command = [
        "/usr/bin/bwrap",
        "--unshare-user", "--unshare-ipc", "--unshare-pid", "--unshare-net",
        "--unshare-uts", "--disable-userns", "--die-with-parent", "--new-session",
        "--uid", uid, "--gid", uid, "--cap-drop", "ALL", "--clearenv",
        "--setenv", "PATH", "/usr/bin:/bin",
        "--setenv", "HOME", "/tmp",
        "--setenv", "TMPDIR", "/tmp",
        "--dev", "/dev", "--tmpfs", "/tmp",
        "--ro-bind", str(Path(__file__).resolve()), SCRIPT,
    ]
    for path in profile["runtime_paths"]:
        if Path(path).exists():
            command.extend(("--ro-bind", path, path))
    for source, destination, writable in profile["mounts"]:
        command.extend(("--bind" if writable else "--ro-bind", source, destination))
    command.extend((
        "--chdir", f"/workspace/agents/{profile['agent']}",
        "--", "/usr/bin/python3", "-I", "-u", SCRIPT,
    ))
    os.execv(command[0], command)


def idle_chat():
    if not AGENT_CONTROLLERS.issubset((CGROUP_ROOT / "cgroup.controllers").read_text().split()):
        raise RuntimeError("guest cgroup is missing required controllers")
    # Cgroup v2 needs the parent to contain no processes before it can
    # delegate memory control. Later Apple execs inherit the init's cgroup.
    control = CGROUP_ROOT / "wakefold-control"
    control.mkdir()
    for pid in (CGROUP_ROOT / "cgroup.procs").read_text().split():
        try:
            (control / "cgroup.procs").write_text(pid)
        except ProcessLookupError:
            pass
    (CGROUP_ROOT / "cgroup.subtree_control").write_text("+memory +pids")
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    signal.pause()


def enter_cgroup(profile):
    deadline = time.monotonic() + 5
    subtree = CGROUP_ROOT / "cgroup.subtree_control"
    while not AGENT_CONTROLLERS.issubset(subtree.read_text().split()):
        if time.monotonic() >= deadline:
            raise RuntimeError("guest cgroup did not enable the agent controllers")
        time.sleep(0.02)
    group = CGROUP_ROOT / f"wakefold-agent-{profile['agent']}-{os.getpid()}"
    group.mkdir()
    try:
        (group / "memory.max").write_text(str(profile["memory_bytes"]))
        (group / "memory.swap.max").write_text("0")
        (group / "pids.max").write_text(str(profile["max_processes"]))
        (group / "cgroup.procs").write_text(str(os.getpid()))
    except BaseException:
        group.rmdir()
        raise


def load_seccomp():
    library = ctypes.CDLL("libseccomp.so.2")
    library.seccomp_init.argtypes = (ctypes.c_uint32,)
    library.seccomp_init.restype = ctypes.c_void_p
    library.seccomp_rule_add.argtypes = (
        ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int, ctypes.c_uint,
    )
    library.seccomp_syscall_resolve_name.argtypes = (ctypes.c_char_p,)
    library.seccomp_load.argtypes = (ctypes.c_void_p,)
    library.seccomp_release.argtypes = (ctypes.c_void_p,)
    context = library.seccomp_init(0x7FFF0000)  # SCMP_ACT_ALLOW
    if not context:
        raise RuntimeError("cannot initialize seccomp")

    try:
        # Keep tools in the invocation's process group and prevent access to
        # the supervisor's memory or file descriptors.
        for name in (
            "setsid", "setpgid", "ptrace", "process_vm_readv", "process_vm_writev",
            "pidfd_getfd", "pidfd_send_signal", "kill", "tkill", "tgkill",
            "unshare", "setns", "io_uring_setup",
        ):
            syscall = library.seccomp_syscall_resolve_name(name.encode("ascii"))
            if syscall != -1:
                result = library.seccomp_rule_add(context, 0x00050000 | errno.EPERM, syscall, 0)
                if result != 0:
                    raise OSError(-result, f"cannot restrict {name}")
        result = library.seccomp_load(context)
        if result != 0:
            raise OSError(-result, "cannot load seccomp")
    finally:
        library.seccomp_release(context)


def stop_group(process):
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def run_command(request):
    stdout = bytearray()
    stderr = bytearray()
    deadline = time.monotonic() + request["timeout_ms"] / 1000
    with tempfile.TemporaryFile() as inputs:
        inputs.write(json.dumps(request["input"]).encode("utf-8"))
        inputs.seek(0)
        with subprocess.Popen(
            request["argv"], stdin=inputs, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True, preexec_fn=load_seccomp, close_fds=True,
        ) as process, selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ, stdout)
            selector.register(process.stderr, selectors.EVENT_READ, stderr)
            selector.register(sys.stdin, selectors.EVENT_READ)
            try:
                remaining_pipes = 2
                while remaining_pipes or process.poll() is None:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise TimeoutError("sandbox command timed out")
                    if process.poll() is not None:
                        stop_group(process)
                    for key, _ in selector.select(min(0.05, remaining)):
                        chunk = os.read(key.fd, 8192)
                        if key.data is None:
                            raise EOFError("sandbox client disconnected")
                        if not chunk:
                            selector.unregister(key.fileobj)
                            remaining_pipes -= 1
                        else:
                            key.data.extend(chunk)
                            if len(key.data) > MAX_OUTPUT:
                                raise ValueError("sandbox command exceeded the output limit")
            finally:
                stop_group(process)

        if process.returncode != 0:
            detail = stderr[:4096].decode("utf-8", "replace").strip()
            raise ValueError(f"sandbox command exited with status {process.returncode}: {detail}")
        return json.loads(stdout)


def serve():
    # Verify the filter is available before acknowledging the sandbox.
    if subprocess.run(
        [sys.executable, "-I", "-c", "pass"], preexec_fn=load_seccomp,
    ).returncode != 0:
        raise RuntimeError("sandbox seccomp probe failed")
    print('{"ready":true}', flush=True)
    for line in sys.stdin.buffer:
        try:
            request = json.loads(line)
            result = {"value": run_command(request)}
        except EOFError:
            break
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            result = {"error": str(error)}
        print(json.dumps(result, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    if sys.argv[1:] == ["--idle-chat"]:
        idle_chat()
    elif len(sys.argv) == 2:
        exec_sandbox(json.loads(sys.argv[1]))
    else:
        serve()
