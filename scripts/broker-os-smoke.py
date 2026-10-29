#!/usr/bin/env python3
"""Broker isolation against two existing unprivileged Linux OS identities.

Run only in an isolated test container/CI with --allow-user-switch. No accounts
are created, no provider is invoked, and all state lives in a temporary folder.
"""
import argparse
import json
import os
import pathlib
import pwd
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--broker", required=True)
    parser.add_argument("--allow-user-switch", action="store_true")
    args = parser.parse_args()
    if not args.allow_user_switch or os.geteuid() != 0:
        parser.error("Use only as root inside an isolated test container with --allow-user-switch")
    identities = [pwd.getpwnam("nobody"), pwd.getpwnam("daemon")]
    assert identities[0].pw_uid != identities[1].pw_uid and all(u.pw_uid != 0 for u in identities)

    with tempfile.TemporaryDirectory(prefix="cp-os-") as temporary:
        root = pathlib.Path(temporary)
        root.chmod(0o755)
        binary = root / "control-plane-host"
        broker_binary = root / "control-plane-broker"
        shutil.copy2(args.binary, binary)
        shutil.copy2(args.broker, broker_binary)
        binary.chmod(0o755)
        broker_binary.chmod(0o755)
        processes = []
        accounts = []
        tokens = {}

        def child(command, user=None, directory=root):
            options = {"stdin": subprocess.DEVNULL, "stdout": subprocess.DEVNULL, "stderr": subprocess.PIPE,
                       "cwd": directory, "env": {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": str(directory), "RUST_LOG": "error"}}
            if user:
                options.update(user=user.pw_uid, group=user.pw_gid, extra_groups=[])
            process = subprocess.Popen(command, **options)
            processes.append(process)
            return process

        def wait_path(path, process):
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError(process.stderr.read().decode())
                if path.exists():
                    return
                time.sleep(0.05)
            raise TimeoutError("Worker did not expose its IPC socket")

        try:
            for name, user in zip(("alice", "bob"), identities):
                directory = root / name
                directory.mkdir(mode=0o700)
                os.chown(directory, user.pw_uid, user.pw_gid)
                data = directory / "data"
                ipc = directory / "ipc"
                ipc.mkdir(mode=0o700)
                os.chown(ipc, user.pw_uid, user.pw_gid)
                command = [str(binary), "--data", str(data)]
                enrollment = subprocess.run(command + ["enroll"], user=user.pw_uid, group=user.pw_gid,
                    extra_groups=[], cwd=directory, env={"PATH": "/usr/bin:/bin", "HOME": str(directory)},
                    text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True).stdout.strip()
                tokens[name] = enrollment
                server = child(command + ["serve", "--workspace-root", str(directory), "--ipc-socket", str(ipc / "worker.sock"), "--broker-uid", "0"], user, directory)
                wait_path(ipc / "worker.grpc.sock", server)
                accounts.append({"id": name, "name": name, "endpoint": {"transport": "unix", "http_socket": str(ipc / "worker.sock"), "grpc_socket": str(ipc / "worker.grpc.sock"), "uid": user.pw_uid}})

            registry = root / "broker.json"
            registry.write_text(json.dumps({"accounts": accounts}))
            registry.chmod(0o600)
            def unused_port():
                with socket.socket() as stream:
                    stream.bind(("127.0.0.1", 0))
                    return stream.getsockname()[1]
            port, grpc_port = unused_port(), unused_port()
            broker = child([str(broker_binary), "--config", str(registry), "--listen", f"127.0.0.1:{port}", "--grpc-listen", f"127.0.0.1:{grpc_port}"])
            base = f"http://127.0.0.1:{port}"

            def request(path, body=None, token=""):
                request = urllib.request.Request(base + path,
                    data=None if body is None else json.dumps(body).encode(),
                    headers={"Content-Type": "application/json", "X-Device-Token": token})
                with urllib.request.urlopen(request, timeout=5) as response:
                    return json.load(response)
            for _ in range(100):
                if broker.poll() is not None:
                    raise RuntimeError(broker.stderr.read().decode())
                try:
                    request("/healthz")
                    break
                except OSError:
                    time.sleep(0.05)
            else:
                raise TimeoutError("Broker did not start")

            for name in ("alice", "bob"):
                paired = request(f"/accounts/{name}/v1/pair", {"secret": tokens[name], "device_name": "Isolation test"})
                tokens[name] = paired["token"]
                created = request(f"/accounts/{name}/v1/sessions", {"provider": "codex", "workspace": str(root / name), "title": name, "trusted": False, "request_id": name}, tokens[name])
                assert created["title"] == name
            for name, other in (("alice", "bob"), ("bob", "alice")):
                assert len(request(f"/accounts/{name}/v1/sessions", token=tokens[name])) == 1
                try:
                    request(f"/accounts/{other}/v1/sessions", token=tokens[name])
                    raise AssertionError("Cross-account device token was accepted")
                except urllib.error.HTTPError as error:
                    assert error.code == 401
            # Bob cannot bypass the broker and open Alice's owner-only IPC socket.
            check = subprocess.run(["python3", "-c", "import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])", accounts[0]["endpoint"]["http_socket"]],
                user=identities[1].pw_uid, group=identities[1].pw_gid, extra_groups=[], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            assert check.returncode != 0
            print("PASS: two distinct unprivileged OS users, private IPC permissions, broker pairing, cross-account denial")
        finally:
            for process in reversed(processes):
                if process.poll() is None:
                    process.send_signal(signal.SIGINT)
            for process in reversed(processes):
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


if __name__ == "__main__":
    main()
