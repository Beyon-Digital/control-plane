#!/usr/bin/env python3
"""Real-process TLS/enrollment smoke check. No inference calls or GUI input."""
import json
import pathlib
import signal
import socket
import ssl
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/control-plane-host"


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    with tempfile.TemporaryDirectory(prefix="control-plane-smoke-") as temp:
        directory = pathlib.Path(temp)
        tls = directory / "tls"
        subprocess.run(["sh", str(ROOT / "scripts/dev-cert.sh"), str(tls), "localhost"], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        http_port, grpc_port = port(), port()
        data = directory / "data"
        command = [str(BINARY), "--data", str(data)]
        secret = subprocess.check_output(command + ["enroll"], text=True).strip()
        process = subprocess.Popen(command + ["serve", "--workspace-root", str(directory),
            "--listen", f"127.0.0.1:{http_port}", "--grpc-listen", f"127.0.0.1:{grpc_port}",
            "--tls-cert", str(tls / "server.pem"), "--tls-key", str(tls / "server.key")],
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        context = ssl.create_default_context(cafile=str(tls / "ca.pem"))
        anonymous_context = context
        key=directory/"device.key";csr=directory/"device.csr"
        subprocess.run(["openssl","req","-new","-newkey","ec","-pkeyopt","ec_paramgen_curve:P-256","-nodes","-subj","/CN=Fixture","-keyout",str(key),"-out",str(csr)],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        base = f"https://localhost:{http_port}"

        def request(path, body=None, token=""):
            headers = {"Content-Type": "application/json", "X-Device-Token": token}
            req = urllib.request.Request(base + path, data=None if body is None else json.dumps(body).encode(), headers=headers)
            with urllib.request.urlopen(req, context=context, timeout=20) as response:
                return json.load(response)

        try:
            for _ in range(100):
                if process.poll() is not None:
                    raise RuntimeError(process.stderr.read().decode())
                try:
                    request("/healthz")
                    break
                except (OSError, urllib.error.URLError):
                    time.sleep(0.05)
            else:
                raise RuntimeError("Worker did not start")
            try:
                request("/v1/sessions")
                raise AssertionError("Unauthenticated request accepted")
            except urllib.error.HTTPError as error:
                assert error.code == 401
            try:
                request("/v1/pair", {"secret":secret,"device_name":"Missing CSR"})
                raise AssertionError("TLS pairing accepted without proof of possession")
            except urllib.error.HTTPError as error:assert error.code==400
            paired = request("/v1/pair", {"secret": secret, "device_name": "Smoke test", "csr_pem":csr.read_text()})
            token = paired["token"]
            certificate=directory/"device.pem";certificate.write_text(paired["certificate"]["certificate_pem"])
            try:
                request("/v1/host",token=token)
                raise AssertionError("Bound credential worked without a client certificate")
            except urllib.error.HTTPError as error:assert error.code==401
            context = ssl.create_default_context(cafile=str(tls / "ca.pem"));context.load_cert_chain(str(certificate),str(key))
            host = request("/v1/host", token=token)
            assert host["protocol_version"] == 1
            body = {"provider": "codex", "workspace": str(directory), "title": "TLS smoke", "trusted": False, "request_id": "smoke-create"}
            first = request("/v1/sessions", body, token)
            assert request("/v1/sessions", body, token)["id"] == first["id"]
            assert len(request("/v1/sessions", token=token)) == 1
            events = request(f"/v1/sessions/{first['id']}/events", token=token)
            assert events[0]["kind"] == "state"
            print("PASS: real mTLS handshake, proof-of-possession pairing, certificate/token binding, auth rejection, idempotency, durable events")
            print("Provider discovery:", ", ".join(f"{p['name']}={'available' if p['available'] else 'unavailable'}" for p in host["providers"]))
        finally:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
