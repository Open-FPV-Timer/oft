#!/usr/bin/env bash
# Create the OFT repos in the GitHub organization (private) and push every local branch.
# Safe to run again: existing repos and remotes are left as they are.
#
#   oft/bench/push_to_github.sh [org]        default org: Open-FPV-Timer
set -euo pipefail
ORG="${1:-Open-FPV-Timer}"
ROOT="$(dirname "$(dirname "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)")")"

declare -A DESC=(
    [oft]="Open FPV Timer: core library, oftd host daemon, simulator and bench tools"
    [oft-spec]="Open FPV Timer specification and build notes"
    [oft-timer-rp2350]="Open FPV Timer firmware for the RP2350 (Waveshare RP2350-POE-ETH)"
    [oft-node-c5]="Open FPV Timer Node firmware for the ESP32-C5, based on widscan58"
    [oft-node-stm32]="Open FPV Timer Node firmware for RotorHazard-style STM32 timers with RX5808 receivers"
    [oft-hardware]="Open FPV Timer carrier boards and cases"
    [rh-oft]="RotorHazard plugin for Open FPV Timer"
)

for r in oft oft-spec oft-timer-rp2350 oft-node-c5 oft-node-stm32 oft-hardware rh-oft; do
    echo "== $r"
    gh repo view "$ORG/$r" >/dev/null 2>&1 ||
        gh repo create "$ORG/$r" --private --description "${DESC[$r]}"
    git -C "$ROOT/$r" remote get-url origin >/dev/null 2>&1 ||
        git -C "$ROOT/$r" remote add origin "https://github.com/$ORG/$r.git"
    # Use the gh login for this push, whatever credential helpers git is set up with.
    GIT_TERMINAL_PROMPT=0 git -C "$ROOT/$r" -c credential.helper= \
        -c credential.helper='!gh auth git-credential' push -u origin --all
done
gh repo list "$ORG" --json name,visibility --jq '.[] | "\(.name)  \(.visibility)"'
