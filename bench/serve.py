#!/usr/bin/env python3
"""OFT bench server: everything a Windows workstation needs for first hardware tests, over
HTTPS with a self-signed certificate (Web Serial needs a secure page).

    /                       the bring-up guide, with a live Timer panel
    /node/                  the Node web UI (oft-node-c5/web/dist) with our firmware to flash
    /files/oft-timer.uf2    Timer firmware for the RP2350 board
    /files/oft-b1-wiring.pdf
    /api/find               scan this machine's LAN for OFT Timers (port 7270)
    /api/timer/<ip>/<path>  GET/PUT proxy to http://<ip>:7270/oft/v1/<path>, so the HTTPS
                            page can talk to a plain-HTTP Timer

Run with bench/serve.sh. Not hardened: for a trusted LAN only.
"""
import concurrent.futures
import http.server
import ipaddress
import json
import os
import posixpath
import socket
import ssl
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

BENCH = Path(__file__).resolve().parent
ROOT = BENCH.parent.parent  # the folder holding the oft-* repos side by side
SITE = BENCH / "site"
NODE_UI = ROOT / "oft-node-c5" / "web" / "dist"
FILES = {
    "oft-timer.uf2": ROOT / "oft-timer-rp2350" / "oft-timer.uf2",
    "oft-b1-wiring.pdf": ROOT / "oft-spec" / "oft-b1-wiring.pdf",
    # First build with update-over-network (new flash layout), its network update file, and
    # the last single-image build repackaged so it installs over either layout.
    "oft-timer-netupdate.uf2": BENCH / "fw" / "oft-timer-netupdate.uf2",
    "oft-timer-netupdate.bin": BENCH / "fw" / "oft-timer-netupdate.bin",
    "oft-timer-fallback.uf2": BENCH / "fw" / "oft-timer-fallback.uf2",
    "oft-timer-noconsole.bin": BENCH / "fw" / "oft-timer-noconsole.bin",
    "oft-timer-console.uf2": BENCH / "fw" / "oft-timer-console.uf2",
    # The Node firmware the flasher at /node/ installs, for updating a Node through its Timer.
    "oft-node-firmware.bin": NODE_UI / "firmware" / "esp32c5" / "firmware.bin",
    # Case for the protoboard build (oft-hardware/case-b1-proto, built by its build.sh).
    **{f"case/{n}": ROOT / "oft-hardware" / "case-b1-proto" / "out" / n
       for n in ("tray.stl", "cover.stl", "exploded.png", "assembly.png", "assembly-open.png", "assembly-pipe-side.png")},
}
TIMER_PORT = 7270
# What the page may ask a Timer for.
TIMER_PATHS = {"info", "config", "status", "time", "crossings", "samples"}


def lan() -> tuple[str, ipaddress.IPv4Network]:
    """This machine's address on the default route, and the network to scan (at most a /22)."""
    out = subprocess.run(["ip", "-4", "-o", "route", "get", "1.1.1.1"], capture_output=True, text=True).stdout.split()
    dev, src = out[out.index("dev") + 1], out[out.index("src") + 1]
    for line in subprocess.run(["ip", "-4", "-o", "addr", "show", "dev", dev], capture_output=True, text=True).stdout.splitlines():
        cidr = line.split()[3]
        if cidr.startswith(src + "/"):
            net = ipaddress.ip_interface(cidr).network
            if net.prefixlen < 22:
                net = ipaddress.ip_interface(f"{src}/22").network
            return src, net
    return src, ipaddress.ip_interface(f"{src}/24").network


def probe(ip: str) -> dict | None:
    try:
        with socket.create_connection((ip, TIMER_PORT), timeout=0.4):
            pass
    except OSError:
        return None
    try:
        with urllib.request.urlopen(f"http://{ip}:{TIMER_PORT}/oft/v1/info", timeout=2) as r:
            info = json.load(r)
        return {"ip": ip, "id": info.get("id"), "host": info.get("host"), "fw": info.get("fw"), "nodes": len(info.get("nodes", []))}
    except (OSError, ValueError):
        return None  # something else is listening on 7270


def find_timers() -> dict:
    _, net = lan()
    with concurrent.futures.ThreadPoolExecutor(max_workers=256) as pool:
        found = [t for t in pool.map(probe, [str(h) for h in net.hosts()]) if t]
    return {"network": str(net), "timers": found}


class Handler(http.server.SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def send_json(self, code: int, obj) -> None:
        self.send_bytes(code, json.dumps(obj).encode(), "application/json")

    def send_bytes(self, code: int, body: bytes, ctype: str) -> None:
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def timer_proxy(self, method: str) -> None:
        url = urllib.parse.urlparse(self.path)
        parts = url.path.split("/")  # ['', 'api', 'timer', ip, path]
        try:
            ip = ipaddress.ip_address(parts[3])
            path = parts[4]
        except (IndexError, ValueError):
            return self.send_json(400, {"error": "use /api/timer/<ip>/<path>"})
        if not (ip.is_private or ip.is_loopback) or path not in TIMER_PATHS or len(parts) != 5:
            return self.send_json(403, {"error": "not allowed"})
        body = None
        if method == "PUT":
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        target = f"http://{ip}:{TIMER_PORT}/oft/v1/{path}" + (f"?{url.query}" if url.query else "")
        req = urllib.request.Request(target, data=body, method=method, headers={"Content-Type": "application/json", "Connection": "close"})
        try:
            with urllib.request.urlopen(req, timeout=3) as r:
                # Timer firmware 0.1.0 ends a list with ",]" when it runs out of buffer.
                self.send_bytes(r.status, r.read().replace(b",]", b"]"), "application/json")
        except urllib.error.HTTPError as e:
            self.send_bytes(e.code, e.read(), "application/json")
        except OSError as e:
            self.send_json(502, {"error": f"Timer at {ip} did not answer: {e}"})

    def do_GET(self):
        path = urllib.parse.urlparse(self.path).path
        if path == "/api/find":
            return self.send_json(200, find_timers())
        if path.startswith("/api/timer/"):
            return self.timer_proxy("GET")
        if path == "/node":
            self.send_response(301)
            self.send_header("Location", "/node/")
            self.send_header("Content-Length", "0")
            return self.end_headers()
        super().do_GET()

    def do_POST(self):
        # /api/bootsel/<ip>: ask the Timer to restart into USB boot mode (the RP2350 drive).
        parts = urllib.parse.urlparse(self.path).path.split("/")
        try:
            ip = ipaddress.ip_address(parts[3]) if parts[1:3] == ["api", "bootsel"] and len(parts) == 4 else None
        except ValueError:
            ip = None
        if ip is None or not (ip.is_private or ip.is_loopback):
            return self.send_json(403, {"error": "not allowed"})
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        req = urllib.request.Request(f"http://{ip}:{TIMER_PORT}/oft/v1/fw/bootsel", data=b"", method="POST", headers={"Connection": "close"})
        try:
            with urllib.request.urlopen(req, timeout=3) as r:
                self.send_bytes(r.status, r.read(), "application/json")
        except urllib.error.HTTPError as e:
            self.send_json(e.code, {"error": "this Timer's firmware has no update-mode command yet; use the BOOT button once"})
        except OSError as e:
            self.send_json(502, {"error": f"Timer at {ip} did not answer: {e}"})

    def do_PUT(self):
        if urllib.parse.urlparse(self.path).path.startswith("/api/timer/"):
            return self.timer_proxy("PUT")
        self.send_json(405, {"error": "method not allowed"})

    def translate_path(self, path):
        path = posixpath.normpath(urllib.parse.unquote(urllib.parse.urlparse(path).path))
        parts = [p for p in path.split("/") if p not in ("", ".", "..")]
        if parts[:1] == ["files"]:
            return str(FILES.get("/".join(parts[1:]), SITE / "missing"))
        if parts[:1] == ["node"]:
            return str(NODE_UI.joinpath(*parts[1:]))
        return str(SITE.joinpath(*parts))

    def guess_type(self, path):
        if str(path).endswith((".uf2", ".bin")):
            return "application/octet-stream"
        return super().guess_type(path)

    def log_message(self, fmt, *args):
        if "/api/timer/" not in (args[0] if args and isinstance(args[0], str) else ""):
            sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, addr, ctx):
        super().__init__(addr, Handler)
        self.ctx = ctx

    def get_request(self):
        # Handshake in the connection's own thread, so one stalled client cannot block accept.
        sock, addr = self.socket.accept()
        return self.ctx.wrap_socket(sock, server_side=True, do_handshake_on_connect=False), addr

    def handle_error(self, request, client_address):
        if not isinstance(sys.exc_info()[1], (ssl.SSLError, ConnectionError, TimeoutError)):
            super().handle_error(request, client_address)


def main() -> None:
    port, cert, key = int(sys.argv[1]), sys.argv[2], sys.argv[3]
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(cert, key)
    os.chdir(SITE)
    print(f"OFT bench server: https://{lan()[0]}:{port}/", flush=True)
    Server(("0.0.0.0", port), ctx).serve_forever()


if __name__ == "__main__":
    main()
