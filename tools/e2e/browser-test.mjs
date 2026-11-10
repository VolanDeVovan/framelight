// Opens the viewer page in headless Chromium tabs and reports what each one decoded.
//   node browser-test.mjs URL [--tabs N] [--seconds S] [--bitrate KBPS] [--screenshot FILE.png]
//                             [--chromium PATH]
// Exit status 0 only if every tab decoded video. Chromium runs with --mute-audio and a
// throwaway profile: nothing is played and nothing of the user's browser is touched.
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const args = process.argv.slice(2);
const url = args.find((a) => !a.startsWith("--"));
const opt = (name, fallback) => {
  const i = args.indexOf("--" + name);
  return i >= 0 ? args[i + 1] : fallback;
};
if (!url) {
  console.error("usage: browser-test.mjs URL [--tabs N] [--seconds S] [--bitrate KBPS] [--screenshot FILE.png] [--chromium PATH]");
  process.exit(2);
}
const tabs = +opt("tabs", 1), seconds = +opt("seconds", 10), bitrate = opt("bitrate", null);
const port = 9300 + Math.floor(Math.random() * 500);
const profile = mkdtempSync(join(tmpdir(), "fl-e2e-"));
const chromium = spawn(opt("chromium", "chromium"), [
  "--headless=new", "--mute-audio", "--no-first-run", "--disable-gpu", "--autoplay-policy=no-user-gesture-required",
  "--window-size=1280,720",
  `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, "about:blank",
], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const cleanup = () => { chromium.kill(); try { rmSync(profile, { recursive: true, force: true }); } catch {} };

async function cdp(wsUrl) {
  const ws = new WebSocket(wsUrl);
  await new Promise((ok, fail) => { ws.onopen = ok; ws.onerror = fail; });
  let id = 0;
  const waiting = new Map();
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (waiting.has(msg.id)) { waiting.get(msg.id)(msg); waiting.delete(msg.id); }
  };
  return (method, params = {}) => new Promise((ok) => { waiting.set(++id, ok); ws.send(JSON.stringify({ id, method, params })); });
}

try {
  let up = false;
  for (let i = 0; i < 100 && !up; i++) {
    try { await fetch(`http://127.0.0.1:${port}/json/version`); up = true; } catch { await sleep(100); }
  }
  if (!up) throw new Error("chromium did not start");
  const opened = performance.now();
  const pages = [];
  for (let i = 0; i < tabs; i++) {
    const t = await (await fetch(`http://127.0.0.1:${port}/json/new?${encodeURIComponent(url)}`, { method: "PUT" })).json();
    pages.push(await cdp(t.webSocketDebuggerUrl));
  }
  const evaluate = async (send, expression) =>
    (await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true })).result?.result?.value;
  // Time from opening the tab to the first decoded frame, per tab.
  const firstFrame = pages.map(() => null);
  const deadline = performance.now() + seconds * 1000;
  let bitrateSet = false;
  while (performance.now() < deadline) {
    for (const [i, send] of pages.entries()) {
      if (firstFrame[i] === null && (await evaluate(send, "document.getElementById('v').videoWidth")) > 0) {
        firstFrame[i] = Math.round(performance.now() - opened);
      }
    }
    if (bitrate && !bitrateSet && performance.now() > deadline - seconds * 500) {
      bitrateSet = true;
      await evaluate(pages[0], `fetch('/bitrate?kbps=${bitrate}', {method: 'POST'}).then(r => r.status)`);
    }
    await sleep(50);
  }
  // Unmuting the element makes Chromium run the audio pipeline; --mute-audio keeps it silent.
  for (const send of pages) await evaluate(send, "document.getElementById('v').muted = false, 1");
  await sleep(1500);
  const shot = opt("screenshot", null);
  if (shot) {
    const png = (await pages[0]("Page.captureScreenshot", { format: "png" })).result.data;
    writeFileSync(shot, Buffer.from(png, "base64"));
  }
  const results = [];
  for (const [i, send] of pages.entries()) {
    const stats = JSON.parse(await evaluate(send, "JSON.stringify(window.flStats)"));
    const audio = await evaluate(send, `(async () => {
      let a = null;
      (await pc.getStats()).forEach((r) => { if (r.type === 'inbound-rtp' && r.kind === 'audio') a = r; });
      return a ? JSON.stringify({ packets: a.packetsReceived, samples: a.totalSamplesReceived, energy: a.totalAudioEnergy }) : null;
    })()`);
    const codec = await evaluate(send, `(async () => {
      let c = null, v = null; const all = await pc.getStats();
      all.forEach((r) => { if (r.type === 'inbound-rtp' && r.kind === 'video') v = r; });
      if (v) c = all.get(v.codecId);
      return c ? c.mimeType + ' ' + c.sdpFmtpLine : null;
    })()`);
    const status = await evaluate(send, "fetch('/status').then(r => r.text())");
    results.push({ tab: i, firstFrameMs: firstFrame[i], ...stats, codec, audio: audio && JSON.parse(audio), status: JSON.parse(status) });
  }
  console.log(JSON.stringify(results, null, 1));
  cleanup();
  process.exit(results.every((r) => r.framesDecoded > 0) ? 0 : 1);
} catch (e) {
  console.error(e);
  cleanup();
  process.exit(1);
}
