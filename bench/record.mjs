#!/usr/bin/env node
// Record a Timer's full-rate signal and crossings from its WebSocket stream to a JSON-lines file.
//   node bench/record.mjs <timer-ip> <out.jsonl> [seconds]
const [ip, out, secs = "600"] = process.argv.slice(2);
if (!ip || !out) { console.error("usage: record.mjs <timer-ip> <out.jsonl> [seconds]"); process.exit(2); }
import { createWriteStream } from "node:fs";
const f = createWriteStream(out);
const ws = new WebSocket(`ws://${ip}:7270/oft/v1/stream`);
let n = { samples: 0, crossing: 0, status: 0 };
ws.onopen = () => ws.send(JSON.stringify({ type: "sub", crossings: {}, samples: { seats: "all", decimate: 1 }, status: true }));
ws.onmessage = (e) => {
  f.write(e.data + "\n");
  try { const m = JSON.parse(e.data); if (m.type === "samples") n.samples += m.p.length; else if (m.type in n) n[m.type]++; } catch { n.bad = (n.bad || 0) + 1; }
};
const done = (why) => { console.log(why, JSON.stringify(n)); f.end(() => process.exit(0)); };
ws.onerror = (e) => done("error " + (e.message || ""));
ws.onclose = () => done("closed");
setTimeout(() => { ws.close(); done("time up"); }, Number(secs) * 1000);
