#!/usr/bin/env python3
"""Check that a Timer's OFT Protocol replies have the shape every Timer host must give.

The Linux Timer (oftd) and the RP2350 firmware build their replies in separate code, so
they can drift apart. This asks one Timer the same questions and checks every reply against
one description, below. Run it against each kind of Timer after changing either.

  bench/conformance.py http://host:7270 [more Timers...]
  bench/conformance.py --fake          build and start `oftd --fake`, and check that

It is safe on a Timer in use: it reads, writes the config back unchanged, and sends a few
requests that must be refused. It never saves settings, restarts or flashes anything, and
it never claims or hands out control: on a Timer that a client holds control of, it checks
that its own writes are refused and skips the checks that need to write.

Exit status: 0 if every Timer passed. GPL-3.0-or-later.
"""
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request

# ---- reply shapes. A dict is an object with exactly these keys ("?" suffix: may be absent);
# a list is an array of that shape; a tuple is "any of"; None is JSON null.
NUM = (int, float)
OPT_INT = (int, None)
THRESHOLD = ({"mode": "auto", "snr_enter": int, "hysteresis": int}, {"mode": "manual", "enter": int, "exit": int})
SEAT_CONFIG = {"seat": int, "freq_mhz": int, "enabled": bool, "threshold?": THRESHOLD}
CONFIG = {"seats": [SEAT_CONFIG], "threshold": THRESHOLD, "min_lap_ms": int, "exit_hold_ms": int, "pass_window_ms": int}
NODE = {"id": str, "chip": str, "fw": str, "range_mhz": [int], "max_seats": int, "level": {"unit": str, "scale": NUM}, "link": str, "slot?": int, "connected?": bool}
INFO = {
    "id": str, "name": str, "boot_id": str, "proto": int, "host": str, "fw": str, "level": str, "calibrated": bool,
    "nodes": [NODE], "seats": [{"seat": int, "node": (int, None), "rate_hz": int}], "capabilities": dict,
}
LINK = {"gap_samples": int, "crc_errors": int, "uart_errors": OPT_INT, "node_overruns": OPT_INT, "node_hop_errors": OPT_INT, "node_gain": OPT_INT}
SEAT_STATUS = {"seat": int, "node": (str, None), "rate_hz": int, "rssi": OPT_INT, "floor": OPT_INT, "enter": int, "exit": int, "crossing": bool}
NODE_STATUS = {
    "slot": int, "id": (str, None), "connected": bool, "temp_c": (*NUM, None), "clock_synced": bool, "clock_source": str,
    "clock_err_us": (*NUM, None), "drift_ppm": (*NUM, None), "link": LINK,
}
STATUS = {
    "type": "status", "uptime_s": int, "time_us": int, "temp_c": (*NUM, None), "timer_temp_c": (*NUM, None), "dropped": int,
    "clock_err_us": (*NUM, None), "clock_synced": bool, "clock_source": str, "drift_ppm": (*NUM, None), "node_connected": bool,
    "link": LINK, "nodes?": [NODE_STATUS], "seats": [SEAT_STATUS], "clients": int, "config_rev": int, "control": ({"id": int, "name": str}, None),
}
CONTROL = {"holder": OPT_INT, "claims": [{"id": int, "name": str, "holds": bool, "connected": bool}]}
CROSSING = {"type": "crossing", "seq": int, "seat": int, "t_enter": int, "t_exit": int, "t_mid": int, "peak": int, "snr": int, "flags": [str]}
ERROR = {"error": str}
NET = ({"mode": "dhcp"}, {"mode": "static", "address": str, "prefix": int, "gateway?": str, "dns?": str})
PINS = {"nodes": [{"tx": int, "rx": int}], "sync": int, "en": int, "boot": int}


def mismatch(v, shape, at="reply"):
    """Why `v` does not have `shape`, or None."""
    if isinstance(shape, tuple):
        if any(mismatch(v, s, at) is None for s in shape):
            return None
        return f"{at}: {json.dumps(v)[:80]} is none of the allowed forms"
    if shape is None:
        return None if v is None else f"{at}: expected null, got {json.dumps(v)[:60]}"
    if isinstance(shape, str):
        return None if v == shape else f"{at}: expected {shape!r}, got {json.dumps(v)[:60]}"
    if shape is dict:
        return None if isinstance(v, dict) else f"{at}: expected an object"
    if isinstance(shape, type):
        ok = isinstance(v, shape) and not (shape in (int, float) and isinstance(v, bool))
        return None if ok else f"{at}: expected {shape.__name__}, got {json.dumps(v)[:60]}"
    if isinstance(shape, list):
        if not isinstance(v, list):
            return f"{at}: expected an array"
        for k, item in enumerate(v):
            m = mismatch(item, shape[0], f"{at}[{k}]")
            if m:
                return m
        return None
    if not isinstance(v, dict):
        return f"{at}: expected an object, got {json.dumps(v)[:60]}"
    want = {k.rstrip("?"): (s, not k.endswith("?")) for k, s in shape.items()}
    missing = [k for k, (_, required) in want.items() if required and k not in v]
    extra = [k for k in v if k not in want]
    if missing or extra:
        return f"{at}: " + "; ".join(p for p in (missing and f"missing {missing}", extra and f"unexpected {extra}") if p)
    for k, x in v.items():
        m = mismatch(x, want[k][0], f"{at}.{k}")
        if m:
            return m
    return None


class Timer:
    def __init__(self, base):
        self.base = base.rstrip("/") + "/oft/v1"
        self.results = []

    def call(self, path, method="GET", body=None, headers=None):
        headers = {**({"Content-Type": "application/json"} if body is not None else {}), **(headers or {})}
        req = urllib.request.Request(self.base + path, method=method, data=None if body is None else json.dumps(body).encode(), headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=5) as r:
                code, raw = r.status, r.read()
        except urllib.error.HTTPError as e:
            code, raw = e.code, e.read()
        try:
            return code, json.loads(raw) if raw else None
        except ValueError:
            return code, raw[:60].decode(errors="replace")

    def check(self, name, problem):
        self.results.append((name, problem))
        print(f"  {'ok  ' if problem is None else 'FAIL'}  {name}" + (f"\n        {problem}" if problem else ""))

    def expect(self, name, path, code, shape, method="GET", body=None, headers=None):
        got, reply = self.call(path, method, body, headers)
        self.check(name, f"status {got}, expected {code}: {json.dumps(reply)[:100]}" if got != code else mismatch(reply, shape))
        return reply if got == code else None

    def run(self):
        info = self.expect("GET /info", "/info", 200, INFO)
        caps = (info or {}).get("capabilities", {})
        status = self.expect("GET /status", "/status", 200, STATUS) or {}
        held = status.get("control") is not None
        self.expect("GET /time", "/time", 200, {"time_us": int, "boot_id": str})
        self.expect("GET /crossings", "/crossings?since=0", 200, {"boot_id": str, "crossings": [CROSSING]})
        config = self.expect("GET /config", "/config", 200, CONFIG)
        if config and held:
            self.expect("PUT /config without the holder's key is refused", "/config", 423, ERROR, "PUT", config)
            print(f"        ({status['control']['name']} holds control: the checks that write are skipped)")
        elif config:
            self.expect("PUT /config, unchanged", "/config", 200, CONFIG, "PUT", config)
            rev = (self.call("/status")[1] or {}).get("config_rev")
            self.check("config_rev moves with a config write", None if rev != status.get("config_rev") else f"still {rev}")
            again = self.call("/config")[1]
            self.check("the config reads back as written", None if again == config else f"wrote {json.dumps(config)[:70]}, read {json.dumps(again)[:70]}")
            bad = {**config, "seats": [{"seat": 9, "freq_mhz": 5800, "enabled": True}]}
            self.expect("PUT /config with seat 9 is refused", "/config", 400, ERROR, "PUT", bad)
            after = self.call("/config")[1]
            self.check("a refused config changes nothing", None if after == config else "the config changed")
        self.expect("GET /samples outside any history", "/samples?seat=7&from=0&to=1", 404, ERROR)
        # Optional parts: there when announced, 404 when not.
        persist = bool(caps.get("config", {}).get("persist"))
        self.expect("GET /config/save" + ("" if persist else " (not announced)"), "/config/save", 200 if persist else 404, {"saved": bool, "current_is_saved": bool} if persist else ERROR)
        net = "network" in caps
        net_state = {"saved": NET, "using": str, "now": ({"address": str, "prefix": int, "gateway?": str}, None), "restart_needed": bool}
        self.expect("GET /network" + ("" if net else " (not announced)"), "/network", 200 if net else 404, net_state if net else (ERROR, str, None))
        pins = "pins" in caps
        pins_state = {"saved": PINS, "using": PINS, "defaults": PINS, "choices": {"nodes": [{"tx": [int], "rx": [int]}], "out": [int]}, "restart_needed": bool}
        self.expect("GET /pins" + ("" if pins else " (not announced)"), "/pins", 200 if pins else 404, pins_state if pins else (ERROR, str, None))
        control = "control" in caps
        self.expect("GET /control" + ("" if control else " (not announced)"), "/control", 200 if control else 404, CONTROL if control else (ERROR, str, None))
        if control:
            # Choosing the writer is for the Timer's own page, which a browser marks with
            # an Origin naming the Timer. The last two checks send that header themselves
            # to reach the refusals behind it; neither changes anything.
            own_page = {"Origin": self.base.rsplit("/oft/v1", 1)[0]}
            self.expect("POST /control from another client is refused", "/control", 403, ERROR, "POST", {"grant": 4000000000})
            self.expect("POST /control from a page served elsewhere is refused", "/control", 403, ERROR, "POST", {"grant": 4000000000}, {"Origin": "http://elsewhere.invalid"})
            self.expect("POST /control without grant is refused", "/control", 400, (ERROR, str, None), "POST", {}, own_page)
            self.expect("POST /control to a claim that does not exist is refused", "/control", 404, ERROR, "POST", {"grant": 4000000000}, own_page)
        return info


def main():
    args = sys.argv[1:]
    if not args:
        sys.exit(__doc__.strip())
    fake = None
    if "--fake" in args:
        args.remove("--fake")
        root = __file__.rsplit("/bench/", 1)[0]
        subprocess.run(["cargo", "build", "-q", "-p", "oftd"], cwd=root, check=True)
        fake = subprocess.Popen([f"{root}/target/debug/oftd", "--fake", "--no-mdns", "--listen", "127.0.0.1:7399"], stderr=subprocess.DEVNULL)
        time.sleep(3)  # long enough for the fake pilot's first crossing
        args.append("http://127.0.0.1:7399")
    failed = 0
    try:
        for base in args:
            print(base)
            t = Timer(base)
            try:
                info = t.run()
            except OSError as e:
                print(f"  FAIL  cannot reach it: {e}")
                failed += 1
                continue
            bad = sum(1 for _, p in t.results if p)
            failed += bad
            who = f"{info['id']} ({info['host']} {info['fw']})" if info else "?"
            print(f"  {who}: {len(t.results) - bad} of {len(t.results)} checks passed\n")
    finally:
        if fake:
            fake.terminate()
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
