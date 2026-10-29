"""Disposable loopback Rust host for native tests; never runs inference."""
from contextlib import contextmanager
import json
import hashlib
import ipaddress
import pathlib
import os
import signal
import socket
import ssl
import subprocess
import tempfile
import time
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]

def free_ports():
    connections = [socket.socket(), socket.socket()]
    try:
        for connection in connections:
            connection.bind(("127.0.0.1", 0))
        return tuple(connection.getsockname()[1] for connection in connections)
    finally:
        for connection in connections:
            connection.close()

@contextmanager
def native_host(host_ip=None, preview=False):
    with tempfile.TemporaryDirectory(prefix="control-plane-native-") as directory:
        root = pathlib.Path(directory)
        project = root / "ControlPlane"
        project.mkdir()
        context = None
        tls_args = []
        if host_ip:
            address = ipaddress.ip_address(host_ip)
            if address.version != 4 or not address.is_private:
                raise ValueError("The physical test fixture requires a private IPv4 address")
            tls = root / "tls"
            subprocess.run(["sh", str(ROOT / "scripts/dev-cert.sh"), str(tls), "localhost"], check=True,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            extensions = (tls / "server.ext").read_text().replace("subjectAltName=", "subjectAltName=IP:" + str(address) + ",")
            (tls / "server-lan.ext").write_text(extensions)
            subprocess.run(["openssl", "x509", "-req", "-in", str(tls / "server.csr"), "-CA", str(tls / "ca.pem"),
                "-CAkey", str(tls / "ca.key"), "-CAcreateserial", "-out", str(tls / "server.pem"), "-days", "1", "-extfile", str(tls / "server-lan.ext")],
                check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            context = ssl.create_default_context(cafile=str(tls / "ca.pem"))
            tls_args = ["--tls-cert", str(tls / "server.pem"), "--tls-key", str(tls / "server.key")]
        worker_environment = None
        if preview:
            binaries = root / "fixture-bin"
            binaries.mkdir()
            adapter = binaries / "codex"
            adapter.write_bytes((ROOT / "scripts/fixtures/codex_preview.py").read_bytes())
            adapter.chmod(0o700)
            worker_environment = {key: value for key, value in os.environ.items() if not key.startswith('CONTROL_PLANE_') and key not in ('OPENAI_API_KEY','ANTHROPIC_API_KEY')}
            worker_environment['PATH'] = str(binaries) + os.pathsep + '/usr/bin:/bin:/usr/sbin:/sbin'
        http, grpc = free_ports()
        command = [str(ROOT / "target/debug/control-plane-host"), "--data", str(root / "data")]
        secret = subprocess.check_output(command + ["enroll"], text=True).strip()
        bind = "0.0.0.0" if host_ip else "127.0.0.1"
        worker = subprocess.Popen(command + ["serve", "--listen", f"{bind}:{http}",
            "--grpc-listen", f"{bind}:{grpc}", "--workspace-root", str(project), "--name", "Development Mac (test)"] + tls_args,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=worker_environment)
        def request(path, body=None, token=""):
            base = f"https://localhost:{http}" if host_ip else f"http://127.0.0.1:{http}"
            message = urllib.request.Request(base + path,
                data=None if body is None else json.dumps(body).encode(),
                headers={"Content-Type": "application/json", "X-Device-Token": token})
            with urllib.request.urlopen(message, timeout=5, context=context) as response:
                return json.load(response)
        try:
            for _ in range(100):
                if worker.poll() is not None:
                    raise RuntimeError("Disposable fixture worker exited")
                try:
                    request("/healthz")
                    break
                except OSError:
                    time.sleep(.05)
            else:
                raise RuntimeError("Disposable fixture worker did not start")
            pairing = {"secret": secret, "device_name": "Native acceptance fixture"}
            if host_ip:
                key, csr = root / "client.key", root / "client.csr"
                subprocess.run(["openssl", "req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
                    "-subj", "/CN=Native acceptance", "-keyout", str(key), "-out", str(csr)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                pairing["csr_pem"] = csr.read_text()
            pair = request("/v1/pair", pairing)
            token = pair["token"]
            if host_ip:
                certificate = root / "client.pem"
                certificate.write_text(pair["certificate"]["certificate_pem"])
                context = ssl.create_default_context(cafile=str(tls / "ca.pem"))
                context.load_cert_chain(str(certificate), str(key))
            session = request("/v1/sessions", {"provider": "codex", "workspace": str(project),
                "title": "Improve session reconnects", "trusted": False, "request_id": "native-fixture"}, token)
            if preview:
                request('/v1/sessions/'+session['id']+'/prompt', {'text':'Show a reconnect cursor example for this UI preview.','request_id':'preview-turn'}, token)
                for _ in range(100):
                    state = request('/v1/sessions', token=token)[0]['state']
                    if state != 'running':
                        break
                    time.sleep(.05)
                if state != 'idle':
                    raise RuntimeError('Local UI adapter did not complete')
            if host_ip:
                phone_secret = subprocess.check_output(command + ["enroll"], text=True).strip()
                yield {"CONTROL_PLANE_TEST_URL": f"https://{host_ip}:{http}",
                    "CONTROL_PLANE_TEST_GRPC_URL": f"https://{host_ip}:{grpc}", "CONTROL_PLANE_TEST_TOKEN": "",
                    "CONTROL_PLANE_TEST_CA": (tls / "ca.pem").read_text(),
                    "CONTROL_PLANE_TEST_CA_SHA256": hashlib.sha256((tls / "ca.cer").read_bytes()).hexdigest(),
                    "CONTROL_PLANE_TEST_ENROLLMENT": phone_secret, "CONTROL_PLANE_TEST_SESSION_ID": session["id"]}
            else:
                yield {"CONTROL_PLANE_TEST_URL": f"http://127.0.0.1:{http}",
                    "CONTROL_PLANE_TEST_GRPC_URL": f"http://127.0.0.1:{grpc}", "CONTROL_PLANE_TEST_TOKEN": token,
                    "CONTROL_PLANE_TEST_SESSION_ID": session["id"]}
        finally:
            if worker.poll() is None:
                worker.send_signal(signal.SIGINT)
            try:
                worker.wait(timeout=10)
            except subprocess.TimeoutExpired:
                worker.kill()
                worker.wait()
