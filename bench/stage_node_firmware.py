#!/usr/bin/env python3
"""Stage the ESP-IDF build of the OFT Node firmware (oft-node-c5/firmware/build-c5) for the
Node web UI's flasher: copies the three images and writes manifest.json in the format
oft-node-c5/web/src/serial/flasher.ts reads (ESP Web Tools' manifest plus size and sha256).

    bench/stage_node_firmware.py <out dir>
"""
import hashlib
import json
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent / "oft-node-c5"
BUILD = ROOT / "firmware" / "build-c5"
# flasher_args.json section -> name in the staged directory
PARTS = {"bootloader": "bootloader.bin", "partition-table": "partitions.bin", "app": "firmware.bin"}


def git(*args: str) -> str:
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True).stdout.strip()


def main() -> None:
    out = Path(sys.argv[1])
    args = json.loads((BUILD / "flasher_args.json").read_text())
    (out / "esp32c5").mkdir(parents=True, exist_ok=True)
    parts = []
    for section, name in PARTS.items():
        src = BUILD / args[section]["file"]
        data = src.read_bytes()
        shutil.copyfile(src, out / "esp32c5" / name)
        parts.append({"path": f"esp32c5/{name}", "offset": int(args[section]["offset"], 16),
                      "size": len(data), "sha256": hashlib.sha256(data).hexdigest()})
        print(f"{name}: {len(data)} bytes @ {parts[-1]['offset']:#x}")
    manifest = {"name": "Widscan58 + OFT Node", "version": "oft-" + (git("describe", "--tags", "--always", "--dirty") or "dev"),
                "date": git("log", "-1", "--format=%cI") or None,
                "builds": [{"chipFamily": "ESP32-C5", "parts": parts}]}
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("version", manifest["version"])


if __name__ == "__main__":
    main()
