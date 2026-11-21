#!/bin/sh
# Install oftd as a service on a Linux timer (a Raspberry Pi with its Node on the UART).
# Run on the timer itself, with sudo, from a folder holding the `oftd` binary built for it
# (cross/build-pi.sh):
#
#   sudo ./install-service.sh [serial port] [more oftd options]     default port: /dev/ttyAMA0
#
# On a RotorHazard-style board whose Node is an STM32 (NuclearHazard and the like), add the
# GPIOs of its reset and BOOT0 lines so the Timer page can update the Node's firmware:
#
#   sudo ./install-service.sh /dev/ttyAMA0 --stm32-reset-gpio 17 --stm32-boot0-gpio 27
#
# It copies oftd to /usr/local/bin, and installs and starts oftd.service, which runs as the
# user who called sudo. That user needs the `dialout` group for the serial port (and `gpio`
# to update an STM32 Node), and keeps
# the saved settings in ~/.config/oftd/settings.json. Run again to update oftd.
# To remove: sudo systemctl disable --now oftd && sudo rm /etc/systemd/system/oftd.service
# GPL-3.0-or-later.
set -e
PORT="${1:-/dev/ttyAMA0}"
[ $# -gt 0 ] && shift
EXTRA="$*"
RUN_AS="${SUDO_USER:-$(id -un)}"
cd "$(dirname "$0")"
[ "$(id -u)" = 0 ] || { echo "run with sudo" >&2; exit 1; }
[ -x ./oftd ] || { echo "no oftd binary next to this script" >&2; exit 1; }
id -nG "$RUN_AS" | tr ' ' '\n' | grep -qx dialout || echo "warning: $RUN_AS is not in the dialout group and may not be able to open $PORT" >&2

systemctl stop oftd 2>/dev/null || true
pkill -x oftd 2>/dev/null || true   # one started by hand would hold the port
install -m 755 ./oftd /usr/local/bin/oftd
cat > /etc/systemd/system/oftd.service <<UNIT
[Unit]
Description=Open FPV Timer (oftd)
After=network-online.target
Wants=network-online.target

[Service]
User=$RUN_AS
ExecStart=/usr/local/bin/oftd --serial $PORT${EXTRA:+ $EXTRA}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable --now oftd
sleep 2
systemctl --no-pager --lines=4 status oftd | sed -n '1,3p;/oftd\[/p'
