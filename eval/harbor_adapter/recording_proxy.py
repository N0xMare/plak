"""Cell-local HTTPS CONNECT recorder; only structural fields and digests persist.

Never use this for Plak's exact-URL subscription routes. CA/private key and
raw buffers stay in the cell, outside /logs. Upstream TLS is always verified.
"""
from __future__ import annotations

import argparse
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import select
import socket
import ssl
import subprocess
import threading
from urllib.parse import urlsplit

from harbor_adapter.observed_config import json_records, request_config, response_config

LIMIT = 32 * 1024 * 1024
HOP_HEADERS = {"connection", "proxy-connection", "proxy-authorization", "transfer-encoding",
               "keep-alive", "upgrade", "te", "trailer", "content-length"}


def require_hosts(hosts: list[str]) -> list[str]:
    if not hosts or any(not re.fullmatch(r"[a-z0-9]+(?:[.-][a-z0-9]+)*", h) for h in hosts):
        raise ValueError("an exact DNS host allowlist is required")
    return sorted(set(hosts))


def make_certificate(directory: Path, hosts: list[str]) -> None:
    hosts = require_hosts(hosts)
    directory.mkdir(mode=0o700, parents=True, exist_ok=False)
    config = directory / "openssl.cnf"
    config.write_text("[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n"
                      "[dn]\nCN=Harbor cell recorder\n[ext]\nbasicConstraints=critical,CA:TRUE\n"
                      "keyUsage=critical,digitalSignature,keyCertSign\n"
                      "subjectAltName=" + ",".join("DNS:" + h for h in hosts) + "\n")
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-keyout", str(directory / "key.pem"), "-out", str(directory / "ca.pem"),
                    "-config", str(config)], check=True, stdout=subprocess.DEVNULL,
                   stderr=subprocess.DEVNULL)
    (directory / "key.pem").chmod(0o600)


class Recorder:
    def __init__(self, output: Path):
        self.output = output
        self.lock = threading.Lock()
        self.requests = []
        self.incomplete = False

    def fail(self) -> None:
        with self.lock:
            self.incomplete = True
            self._write()

    def begin(self, body: bytes) -> int | None:
        value = json.loads(body)
        if not isinstance(value, dict) or not isinstance(value.get("model"), str):
            return None
        with self.lock:
            index = len(self.requests)
            self.requests.append({"request": request_config(value), "response": {},
                                  "request_sha256": hashlib.sha256(body).hexdigest()})
            self._write()
            return index

    def finish(self, index: int | None, body: bytes, *, status_code=200, transport_error=False, client_cancelled=False) -> None:
        if index is None:
            return
        served = {"status_code": status_code, "transport_error": transport_error, "client_cancelled": client_cancelled}
        for value in json_records(body.decode("utf-8", errors="replace")):
            served.update({k: v for k, v in response_config(value).items() if v is not None})
        with self.lock:
            self.requests[index]["response"] = served
            self._write()

    def _write(self) -> None:
        value = {"schemaVersion": 1, "source": "exchange", "wire_verified": bool(self.requests) and not self.incomplete,
                 "capture_incomplete": self.incomplete,
                 "first_request": self.requests[0]["request"] if self.requests else None,
                 "requests": self.requests}
        temporary = self.output.with_suffix(".tmp")
        with open(temporary, "w", opener=lambda p, f: os.open(p, f, 0o600)) as stream:
            json.dump(value, stream, sort_keys=True)
            stream.write("\n")
        temporary.replace(self.output)


class ProxyServer(ThreadingHTTPServer):
    daemon_threads = True

    def handle_error(self, request, client_address):
        # Exceptions may contain request URLs, headers or content.
        pass


class ProxyHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def __init__(self, *args, tunnel_host=None, **kwargs):
        self.tunnel_host = tunnel_host
        super().__init__(*args, **kwargs)

    def log_message(self, *args):
        pass

    def do_CONNECT(self):
        if self.path not in {host + ":443" for host in self.server.hosts}:
            if getattr(self.server, "passthrough", False):
                self._tunnel()
                return
            self.send_error(403)
            return
        self.send_response(200)
        self.end_headers()
        self.wfile.flush()
        connection = self.server.tls.wrap_socket(self.connection, server_side=True)
        try:
            ProxyHandler(connection, self.client_address, self.server,
                         tunnel_host=self.path.removesuffix(":443"))
        finally:
            connection.close()
            self.close_connection = True

    def _tunnel(self):
        """Non-provider tool traffic stays opaque; no model-content observation."""
        target = urlsplit("//" + self.path)
        try:
            if not target.hostname or target.username or target.password or target.path or target.query or target.fragment:
                raise ValueError
            port = target.port or 443
            upstream = socket.create_connection((target.hostname, port), timeout=15)
        except (OSError, ValueError):
            self.send_error(502)
            return
        try:
            self.send_response(200)
            self.end_headers()
            self.wfile.flush()
            peers = {self.connection: upstream, upstream: self.connection}
            while True:
                ready, _, _ = select.select(list(peers), [], [], 300)
                if not ready:
                    return
                for source in ready:
                    chunk = source.recv(65536)
                    if not chunk:
                        return
                    peers[source].sendall(chunk)
        except OSError:
            pass
        finally:
            upstream.close()
            self.close_connection = True

    def do_POST(self):
        self._forward()

    def do_GET(self):
        self._forward()

    def _forward(self):
        host = self.headers.get("Host", "").removesuffix(":443")
        if (not isinstance(self.connection, ssl.SSLSocket) or host not in self.server.hosts or host != self.tunnel_host
                or urlsplit(self.path).scheme or not self.path.startswith("/")
                or self.headers.get("Transfer-Encoding")):
            self.send_error(403)
            return
        index = None
        captured = bytearray()
        response_status = None
        sending = False
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 <= length <= LIMIT:
                self.server.recorder.fail()
                raise ValueError
            body = self.rfile.read(length)
            if len(body) != length:
                self.server.recorder.fail()
                raise ValueError
            try:
                index = (self.server.recorder.begin(body) if body and self.command == "POST"
                         and urlsplit(self.path).path.endswith(("/responses", "/chat/completions", "/messages"))
                         else None)
            except (ValueError, TypeError):
                self.server.recorder.fail()
                index = None
            headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP_HEADERS}
            headers["Accept-Encoding"] = "identity"
            upstream = http.client.HTTPSConnection(host, timeout=300, context=ssl.create_default_context())
            try:
                upstream.request(self.command, self.path, body, headers)
                response = upstream.getresponse()
                response_status = response.status
                sending = True
                self.send_response(response.status)
                for key, value in response.getheaders():
                    if key.lower() not in HOP_HEADERS:
                        self.send_header(key, value)
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                sending = False
                while chunk := response.read1(65536):
                    if len(captured) + len(chunk) > LIMIT:
                        self.server.recorder.fail()
                        raise ValueError("response too large")
                    captured.extend(chunk)
                    sending = True
                    self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
                    self.wfile.flush()
                    sending = False
                self.server.recorder.finish(index, bytes(captured), status_code=response.status)
                sending = True
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            finally:
                upstream.close()
        except Exception:
            self.server.recorder.finish(index, bytes(captured), status_code=response_status,
                                        transport_error=not sending, client_cancelled=sending)
            self.close_connection = True


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--host", action="append", required=True)
    parser.add_argument("--port", type=int, default=18080)
    parser.add_argument("--passthrough", action="store_true",
                        help="blind-tunnel non-provider HTTPS so native tools keep their network authority")
    args = parser.parse_args()
    hosts = require_hosts(args.host)
    directory = args.directory.resolve()
    if directory.is_relative_to(Path("/logs")):
        parser.error("recorder secrets must not live under /logs")
    os.umask(0o077)
    make_certificate(directory, hosts)
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.minimum_version = ssl.TLSVersion.TLSv1_2
    tls.load_cert_chain(directory / "ca.pem", directory / "key.pem")
    server = ProxyServer(("127.0.0.1", args.port), ProxyHandler)
    server.hosts, server.tls = hosts, tls
    server.passthrough = args.passthrough
    server.recorder = Recorder(directory / "observed-config.json")
    (directory / "ready").write_text(str(server.server_port))
    server.serve_forever()


if __name__ == "__main__":
    main()
