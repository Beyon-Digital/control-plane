#!/usr/bin/env python3
"""Host-local WSL bridge. Install explicitly inside the intended Linux account.

No shell command construction, Windows path conversion, or global distro termination.
The run lease is per process tree; EOF, expiry, cancel, and a broken output pipe
terminate that Linux process group even if the Windows launcher disappears.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import select
import shutil
import signal
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid

MAX_TEXT = 2 * 1024 * 1024
MAX_OUTPUT = 8 * 1024 * 1024
LEASE_SECONDS = 20
LEASE_DIR = Path.home() / ".cache/control-plane/wsl-leases"


def emit(value):
    print(json.dumps(value, ensure_ascii=False), flush=True)


def linux_path(value):
    if not isinstance(value, str) or not value.startswith("/") or "\\" in value or "\0" in value:
        raise ValueError("an absolute Linux path is required")
    if ".." in PurePosixPath(value).parts or value == "/" or value.startswith("/mnt/"):
        raise ValueError("use a Linux filesystem workspace, without parent traversal")
    return Path(value)


def workspace(config):
    root = linux_path(config["workspace"]).resolve(strict=True)
    roots = [linux_path(value).resolve(strict=True) for value in config["roots"]]
    if not root.is_dir() or not any(root == allowed or allowed in root.parents for allowed in roots):
        raise ValueError("workspace is outside distribution roots")
    return root


def relative(root, value):
    if not isinstance(value, str) or "\\" in value or "\0" in value:
        raise ValueError("invalid relative path")
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts:
        raise ValueError("path must stay within workspace")
    current = root
    for part in path.parts:
        current = current / part
        if current.is_symlink():
            raise ValueError("symlinks are not exposed through the file API")
    return current


def read_text(path):
    with path.open("rb") as source:
        data = source.read(MAX_TEXT + 1)
    if len(data) > MAX_TEXT or b"\0" in data:
        raise ValueError("binary file or text exceeds 2 MiB")
    return data, data.decode("utf-8")


def lease_path(identifier):
    if str(uuid.UUID(identifier)) != identifier:
        raise ValueError("invalid lease identifier")
    LEASE_DIR.mkdir(mode=0o700, parents=True, exist_ok=True)
    if LEASE_DIR.is_symlink() or LEASE_DIR.stat().st_uid != os.getuid():
        raise ValueError("invalid lease directory ownership")
    LEASE_DIR.chmod(0o700)
    return LEASE_DIR / (identifier + ".json")


def process_start(pid):
    # Linux proc stat field 22. Split after final ')' to handle spaces in comm.
    return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19]


def kill_tree(record):
    try:
        pid = int(record["pid"])
        if process_start(pid) == record["start"]:
            os.killpg(pid, signal.SIGKILL)
    except (FileNotFoundError, ProcessLookupError):
        pass


def update_lease(identifier, rows=None, cols=None):
    path = lease_path(identifier)
    with path.open("r+", encoding="utf-8") as source:
        import fcntl
        fcntl.flock(source, fcntl.LOCK_EX)
        record = json.load(source)
        if rows is not None:
            if not 1 <= rows <= 500 or not 1 <= cols <= 500:
                raise ValueError("invalid terminal size")
            record["rows"], record["cols"] = rows, cols
        record["renewed"] = time.monotonic()
        source.seek(0)
        json.dump(record, source)
        source.truncate()
        source.flush()


def load_lease(path):
    import fcntl
    with path.open(encoding="utf-8") as source:
        fcntl.flock(source, fcntl.LOCK_SH)
        return json.load(source)


def environment():
    env = dict(os.environ)
    # Provider login/configuration stays in this Linux account. Optional API
    # credentials are loaded locally, never transported on Windows argv.
    credentials = Path.home() / ".config/control-plane/environment.json"
    if credentials.exists():
        info = credentials.stat()
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise ValueError("environment.json must be an owner-only regular file")
        values = json.loads(credentials.read_text())
        if not isinstance(values, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in values.items()):
            raise ValueError("environment.json must contain string environment values")
        env.update(values)
    env["PATH"] = str(Path.home() / ".local/bin") + os.pathsep + env.get("PATH", "/usr/local/bin:/usr/bin:/bin")
    return env


def run(config, identifier):
    import fcntl
    import termios
    root = workspace(config)
    terminal = config.get("terminal", False)
    argv = config.get("argv")
    if not isinstance(argv, list) or not argv or not all(isinstance(a, str) and "\0" not in a for a in argv):
        raise ValueError("invalid argv")
    env = environment()
    stopped = threading.Event()
    master = None
    if terminal:
        import pty
        master, slave = pty.openpty()
        def terminal_session():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        child = subprocess.Popen(argv, cwd=root, env=env, stdin=slave, stdout=slave, stderr=slave, preexec_fn=terminal_session, close_fds=True)
        os.close(slave)
    else:
        child = subprocess.Popen(argv, cwd=root, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=sys.stderr, start_new_session=True)
    path = lease_path(identifier)
    record = {"pid": child.pid, "start": process_start(child.pid), "renewed": time.monotonic(), "rows": 24, "cols": 80}
    try:
        with path.open("x", encoding="utf-8") as dest:
            os.chmod(path, 0o600)
            json.dump(record, dest)
    except BaseException:
        os.killpg(child.pid, signal.SIGKILL)
        child.wait()
        raise
    input_fd = child.stdin.fileno() if master is None else master
    output_fd = child.stdout.fileno() if master is None else master
    def copy_input():
        try:
            while not stopped.is_set():
                data = os.read(sys.stdin.fileno(), 8192)
                if not data:
                    break
                view = memoryview(data)
                while view:
                    view = view[os.write(input_fd, view):]
        except (BrokenPipeError, OSError):
            pass
        finally:
            stopped.set()
    def copy_output():
        try:
            while not stopped.is_set():
                data = os.read(output_fd, 8192)
                if not data:
                    break
                view = memoryview(data)
                while view:
                    view = view[os.write(sys.stdout.fileno(), view):]
        except (BrokenPipeError, OSError):
            stopped.set()
    inbound = threading.Thread(target=copy_input, daemon=True)
    outbound = threading.Thread(target=copy_output, daemon=True)
    inbound.start()
    outbound.start()
    geometry = None
    try:
        while child.poll() is None and not stopped.wait(0.2):
            current = load_lease(path)
            if time.monotonic() - current["renewed"] > LEASE_SECONDS:
                raise RuntimeError("Windows host lease expired")
            size = (current["rows"], current["cols"])
            if master is not None and geometry != size:
                fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", *size, 0, 0))
                geometry = size
        status = child.poll()
        if status is None:
            kill_tree(record)
            child.wait()
            status = child.returncode
        outbound.join(timeout=2)
        return status if status >= 0 else 128 - status
    finally:
        # Also reap background descendants after a provider exits successfully.
        try:
            # This helper owns the group. A group with live descendants retains
            # its PGID even if its leader has already exited.
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        if child.poll() is None:
            child.wait()
        path.unlink(missing_ok=True)
        if master is not None:
            os.close(master)


def bounded_git(root, args):
    allowed = {"status", "diff", "log", "show", "add", "commit", "push", "worktree", "rev-parse", "branch"}
    if not isinstance(args, list) or not args or args[0] not in allowed or not all(isinstance(a, str) and "\0" not in a for a in args):
        raise ValueError("unsupported Git operation")
    env = environment()
    env["GIT_TERMINAL_PROMPT"] = "0"
    child = subprocess.Popen(["git", "--no-pager", *args], cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    chunks = {child.stdout: bytearray(), child.stderr: bytearray()}
    deadline = time.monotonic() + 120
    active = list(chunks)
    try:
        while active:
            if time.monotonic() > deadline:
                raise TimeoutError("Git timed out")
            ready, _, _ = select.select(active, [], [], 0.2)
            for source in ready:
                data = os.read(source.fileno(), 8192)
                if not data:
                    active.remove(source)
                    continue
                chunks[source].extend(data)
                if len(chunks[source]) > MAX_OUTPUT:
                    raise ValueError("Git output exceeds 8 MiB per stream")
        code = child.wait(timeout=max(0.1, deadline-time.monotonic()))
        return {"success":code == 0,"exit_code":code,"stdout":chunks[child.stdout].decode("utf-8", errors="replace"),"stderr":chunks[child.stderr].decode("utf-8", errors="replace")}
    finally:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait()


def request(config):
    operation = config["operation"]
    if operation.startswith("transfer_"):
        from wsl_transfer import transfer_request
        return transfer_request(config, workspace(config))
    if operation == "discover":
        env = environment()
        providers = []
        for name, executable in [("codex", "codex"), ("kiro", "kiro-cli"), ("claude", "node")]:
            version = None
            found = shutil.which(executable, path=env["PATH"])
            if found:
                try:
                    result = subprocess.run([found, "--version"], env=env, capture_output=True, timeout=3, check=True)
                    version = result.stdout[:1024].decode(errors="replace").strip()
                except (subprocess.SubprocessError, OSError):
                    pass
            available = version is not None
            detail = "Linux runtime in selected distribution"
            if name == "claude":
                sidecar = config.get("claude_sidecar")
                available = available and bool(sidecar) and Path(sidecar).is_file() and (Path(sidecar).parent.parent / "node_modules/@anthropic-ai/claude-agent-sdk/package.json").is_file()
                detail = "Requires the installed Linux Claude sidecar and Linux-account API credentials"
            providers.append({"name":name,"available":available,"version":version,"detail":detail})
        return {"providers":providers,"user":os.getuid(),"bridge_version":1}
    root = workspace(config)
    if operation == "validate":
        return {"workspace":str(root)}
    if operation == "git":
        return bounded_git(root, config["args"])
    path = relative(root, config.get("path", ""))
    if operation == "resolve":
        return {"path":str(path)}
    if operation == "list":
        entries = []
        for item in path.iterdir():
            if item.is_symlink() or item.name in (".git", ".control-plane-transfers"):
                continue
            entries.append({"name":item.name,"is_dir":item.is_dir(),"size":item.stat().st_size,"path":str(PurePosixPath(config.get("path", "")) / item.name)})
            if len(entries) > 10000:
                raise ValueError("directory exceeds 10000 entries")
        return {"path":config.get("path", ""),"entries":sorted(entries,key=lambda item:item["name"])}
    if operation == "read":
        data, content = read_text(path)
        return {"path":config["path"],"content":content,"sha256":hashlib.sha256(data).hexdigest()}
    if operation == "write":
        content = config["content"].encode("utf-8")
        if len(content) > MAX_TEXT or b"\0" in content or path == root:
            raise ValueError("invalid text file")
        existing = read_text(path)[0] if path.exists() else None
        expected = config.get("expected_sha256")
        if (existing is None and expected is not None) or (existing is not None and expected != hashlib.sha256(existing).hexdigest()):
            raise ValueError("conflict: file revision does not match")
        fd, temporary = tempfile.mkstemp(prefix=".control-plane-", dir=path.parent)
        try:
            with os.fdopen(fd, "wb") as dest:
                if existing is not None:
                    os.fchmod(dest.fileno(), stat.S_IMODE(path.stat().st_mode))
                dest.write(content)
                dest.flush()
                os.fsync(dest.fileno())
            current = read_text(path)[0] if path.exists() else None
            if current != existing:
                raise ValueError("conflict: file changed during save")
            os.replace(temporary, path)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
        return {"path":config["path"],"sha256":hashlib.sha256(content).hexdigest()}
    raise ValueError("unsupported bridge operation")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["request", "run", "renew", "cancel", "resize"])
    parser.add_argument("--config")
    parser.add_argument("--lease")
    parser.add_argument("--rows", type=int)
    parser.add_argument("--cols", type=int)
    args = parser.parse_args()
    if args.mode == "renew":
        update_lease(args.lease)
    elif args.mode == "resize":
        update_lease(args.lease, args.rows, args.cols)
    elif args.mode == "cancel":
        path = lease_path(args.lease)
        if path.exists():
            kill_tree(load_lease(path))
    elif args.mode == "run":
        config = json.loads(base64.urlsafe_b64decode(args.config))
        return run(config, args.lease)
    else:
        payload = sys.stdin.buffer.read(MAX_TEXT * 2 + 1)
        if len(payload) > MAX_TEXT * 2:
            raise ValueError("bridge request exceeds 4 MiB")
        emit({"ok":True,"result":request(json.loads(payload))})
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        # Do not print request payloads or credential-bearing environment values.
        if len(sys.argv) > 1 and sys.argv[1] == "request":
            emit({"ok":False,"error":str(error)})
        else:
            print(f"WSL bridge: {error}", file=sys.stderr)
        sys.exit(1)
