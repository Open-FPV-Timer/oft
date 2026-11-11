#!/usr/bin/env bash
# OFT bench server (see serve.py). Self-signed HTTPS on the LAN.
#
#   bench/serve.sh start     build what is missing, start in the background
#   bench/serve.sh stop
#   bench/serve.sh status
#   bench/serve.sh rebuild   restage the Node firmware, rebuild the Node web UI and the Timer UF2
#   bench/serve.sh run       run in the foreground (Ctrl-C to stop)
set -euo pipefail
BENCH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$(dirname "$BENCH")")"   # the folder holding the oft-* repos side by side
PORT="${PORT:-8443}"
RUN="$BENCH/.run"
CERT="$RUN/cert.pem"; KEY="$RUN/key.pem"; PID="$RUN/server.pid"; LOG="$RUN/server.log"
mkdir -p "$RUN"

ip_addr() { ip -4 -o route get 1.1.1.1 | sed -n 's/.* src \([0-9.]*\).*/\1/p'; }

build() {
    echo ">> Node firmware -> web UI flasher"
    python3 "$BENCH/stage_node_firmware.py" "$ROOT/oft-node-c5/web/public/firmware"
    echo ">> Node web UI"
    (cd "$ROOT/oft-node-c5/web" && { [ -d node_modules ] || npm ci --no-audit --no-fund; } && npm run build >/dev/null)
    echo ">> Timer UF2"
    (cd "$ROOT/oft-timer-rp2350" && cargo build --release -q &&
        python3 tools/uf2.py target/thumbv8m.main-none-eabihf/release/oft-timer-rp2350 oft-timer.uf2)
}

prepare() {
    if [ ! -f "$ROOT/oft-node-c5/web/dist/firmware/manifest.json" ] || [ ! -f "$ROOT/oft-timer-rp2350/oft-timer.uf2" ]; then
        build
    fi
    local ip; ip="$(ip_addr)"
    if [ ! -f "$CERT" ] || ! openssl x509 -in "$CERT" -noout -text | grep -q "IP Address:$ip"; then
        echo ">> Self-signed certificate for $ip"
        openssl req -x509 -newkey rsa:2048 -nodes -keyout "$KEY" -out "$CERT" -days 365 \
            -subj "/CN=oft-bench" -addext "subjectAltName=IP:$ip,IP:127.0.0.1,DNS:localhost" >/dev/null 2>&1
    fi
}

running() { [ -f "$PID" ] && kill -0 "$(cat "$PID")" 2>/dev/null; }

case "${1:-start}" in
    start)
        if running; then echo "already running: https://$(ip_addr):$PORT/"; exit 0; fi
        prepare
        setsid nohup python3 "$BENCH/serve.py" "$PORT" "$CERT" "$KEY" >"$LOG" 2>&1 </dev/null &
        echo $! >"$PID"
        sleep 1
        running || { echo "failed to start:"; cat "$LOG"; exit 1; }
        echo "running: https://$(ip_addr):$PORT/   (log: $LOG)"
        ;;
    stop)
        if running; then kill "$(cat "$PID")"; echo "stopped"; else echo "not running"; fi
        rm -f "$PID"
        ;;
    status)
        if running; then echo "running: https://$(ip_addr):$PORT/"; else echo "not running"; fi
        ;;
    rebuild) build ;;
    run) prepare; exec python3 "$BENCH/serve.py" "$PORT" "$CERT" "$KEY" ;;
    *) sed -n '2,9p' "${BASH_SOURCE[0]}"; exit 2 ;;
esac
