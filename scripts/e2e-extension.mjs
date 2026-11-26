// End-to-end check of the browser integration, with a real Chromium.
//
//   node scripts/e2e-extension.mjs <chromium-binary> [--keep]
//
// Starts the test server and the application in a scratch directory, loads
// the unpacked extension into a throwaway browser profile, then:
//
//   1. clicks a download link and expects the file to arrive through Siphon
//      (and not through the browser);
//   2. opens a page that requests an HLS stream, expects the popup to list
//      it, and sends it to Siphon from there;
//   3. saves a screenshot of the popup;
//   4. checks the capture rules: Alt held while clicking, a file type left to
//      the browser, and a site left to the browser;
//   5. opens a page with a video, expects the download button by the player,
//      clicks it and expects the video to arrive through Siphon;
//   6. sends every link of a page to the application's link picker.
//
// Nothing here touches the user's own browser profile or Siphon library.

import { spawn } from "node:child_process";
import { mkdirSync, writeFileSync, readFileSync, existsSync, rmSync, readdirSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const chromium = process.argv[2];
const keep = process.argv.includes("--keep");
if (!chromium) {
  console.error("usage: node scripts/e2e-extension.mjs <chromium-binary> [--keep]");
  process.exit(2);
}

const EXTENSION_ID = "epjmjlofaaebaippnkglkkifcdgpgmfp";
const SERVER_PORT = 8791;
const DEBUG_PORT = 9347;
const base = `http://127.0.0.1:${SERVER_PORT}`;

const work = join(tmpdir(), `siphon-e2e-${process.pid}`);
const dirs = { profile: join(work, "profile"), data: join(work, "data"), files: join(work, "files"), browser: join(work, "browser-downloads") };
for (const dir of Object.values(dirs)) mkdirSync(dir, { recursive: true });
mkdirSync(join(dirs.profile, "NativeMessagingHosts"), { recursive: true });

const bin = (name) => join(root, "target", "debug", name);
writeFileSync(
  join(dirs.profile, "NativeMessagingHosts", "app.siphon.host.json"),
  JSON.stringify({ name: "app.siphon.host", description: "Siphon (test)", path: bin("siphon-host"), type: "stdio", allowed_origins: [`chrome-extension://${EXTENSION_ID}/`] }),
);
writeFileSync(
  join(dirs.data, "settings.json"),
  JSON.stringify({ download_dir: dirs.files, save_automatically: true, focus_on_capture: false, net: { proxy: { mode: "direct" } } }),
);

// The browser saves to a scratch folder, never to the user's Downloads. This
// is set as an ordinary profile preference: telling the browser through the
// debug protocol to manage downloads would bypass the very extension hook
// under test.
mkdirSync(join(dirs.profile, "Default"), { recursive: true });
writeFileSync(
  join(dirs.profile, "Default", "Preferences"),
  JSON.stringify({ download: { default_directory: dirs.browser, prompt_for_download: false, directory_upgrade: true }, profile: { default_content_setting_values: { automatic_downloads: 1 } } }),
);

const children = [];
function start(command, args, env = {}) {
  const child = spawn(command, args, { env: { ...process.env, ...env }, stdio: ["ignore", "pipe", "pipe"] });
  children.push(child);
  return child;
}
function cleanup() {
  for (const child of children) child.kill("SIGTERM");
  if (!keep) {
    try {
      rmSync(work, { recursive: true, force: true, maxRetries: 8, retryDelay: 250 });
    } catch {
      console.error(`could not remove ${work}; delete it by hand`);
    }
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(what, test, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await test();
    if (value) return value;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await sleep(150);
  }
}

/** Minimal Chrome DevTools Protocol client over one browser socket. */
class Browser {
  static async connect(port) {
    const info = await until("the browser's debug port", async () => {
      try {
        return await (await fetch(`http://127.0.0.1:${port}/json/version`)).json();
      } catch {
        return null;
      }
    });
    const socket = new WebSocket(info.webSocketDebuggerUrl);
    await new Promise((ok, fail) => {
      socket.onopen = ok;
      socket.onerror = () => fail(new Error("could not open the debug socket"));
    });
    return new Browser(socket);
  }

  constructor(socket) {
    this.socket = socket;
    this.nextId = 1;
    this.waiting = new Map();
    socket.onmessage = (event) => {
      const message = JSON.parse(event.data);
      const waiter = this.waiting.get(message.id);
      if (!waiter) return;
      this.waiting.delete(message.id);
      message.error ? waiter.reject(new Error(message.error.message)) : waiter.resolve(message.result);
    };
  }

  send(method, params = {}, sessionId) {
    const id = this.nextId++;
    this.socket.send(JSON.stringify({ id, method, params, sessionId }));
    return new Promise((resolve, reject) => this.waiting.set(id, { resolve, reject }));
  }

  async open(url) {
    const { targetId } = await this.send("Target.createTarget", { url });
    const { sessionId } = await this.send("Target.attachToTarget", { targetId, flatten: true });
    await this.send("Page.enable", {}, sessionId);
    await this.send("Runtime.enable", {}, sessionId);
    return { targetId, sessionId };
  }

  async evaluate(page, expression) {
    const { result, exceptionDetails } = await this.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true }, page.sessionId);
    if (exceptionDetails) throw new Error(exceptionDetails.exception?.description ?? exceptionDetails.text);
    return result.value;
  }
}

function library() {
  try {
    return JSON.parse(readFileSync(join(dirs.data, "downloads.json"), "utf8")).downloads ?? [];
  } catch {
    return [];
  }
}

const checks = [];
function check(name, ok, detail = "") {
  checks.push({ name, ok });
  console.log(`${ok ? "ok  " : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
}

try {
  start(bin("siphon-testserver"), ["--port", String(SERVER_PORT)]);
  await until("the test server", async () => fetch(`${base}/`).then((r) => r.ok, () => false));

  // The application is deliberately NOT started here: the first capture
  // must make the native host launch it.
  const browser = start(
    chromium,
    [
      "--headless=new",
      `--user-data-dir=${dirs.profile}`,
      `--load-extension=${join(root, "extension")}`,
      `--disable-extensions-except=${join(root, "extension")}`,
      `--remote-debugging-port=${DEBUG_PORT}`,
      "--no-first-run",
      "--no-default-browser-check",
      "--disable-features=DisableLoadExtensionCommandLineSwitch",
      "about:blank",
    ],
    { SIPHON_DATA_DIR: dirs.data },
  );
  browser.stderr.on("data", () => {});
  const cdp = await Browser.connect(DEBUG_PORT);

  // The extension must load under the ID the native host manifest allows.
  const worker = await until("the extension's service worker", async () => {
    const { targetInfos } = await cdp.send("Target.getTargets");
    return targetInfos.find((t) => t.type === "service_worker" && t.url.startsWith(`chrome-extension://${EXTENSION_ID}/`));
  });
  check("extension loads with its pinned ID", Boolean(worker), worker?.url);

  // 1. A clicked download link goes to Siphon.
  const page = await cdp.open(`${base}/`);
  await until("the test page", () => cdp.evaluate(page, "document.readyState === 'complete'"));
  await cdp.evaluate(page, "document.getElementById('Annual-Report-2026.pdf').click()");

  const report = await until("Siphon to finish the captured download", () => library().find((d) => d.file_name === "Annual-Report-2026.pdf" && d.status.state === "completed"), 45_000);
  check("the native host started the application", existsSync(join(dirs.data, "endpoint.json")));
  check("a clicked link is downloaded by Siphon", Boolean(report), `${report.total} bytes`);
  const saved = join(dirs.files, "Annual-Report-2026.pdf");
  check("the file is complete on disk", existsSync(saved) && statSync(saved).size === 14 * 1024 * 1024, saved);
  check("the browser did not keep the download", readdirSync(dirs.browser).filter((f) => !f.endsWith(".crdownload")).length === 0, readdirSync(dirs.browser).join(", ") || "empty");
  check("the page it came from is remembered", report.request.referer?.startsWith(base) === true, report.request.referer);
  check("the browser's identity was passed on", (report.request.headers ?? []).some(([n, v]) => n === "User-Agent" && /Chrome\//.test(v)));

  // A name that only the response header knows.
  await cdp.evaluate(page, "document.getElementById('named').click()");
  const named = await until("the header-named download", () => library().find((d) => d.file_name === "Quarterly Figures (final).xlsx" && d.status.state === "completed"), 30_000);
  check("the file name comes from the server's header", Boolean(named));

  // 2. Media sniffing and the popup.
  const watch = await cdp.open(`${base}/watch`);
  await until("the stream to be requested", () => cdp.evaluate(watch, "document.getElementById('state').textContent === 'stream requested'"));
  await sleep(600);

  // Any extension page can talk to the extension's own APIs.
  const probe = await cdp.open(`chrome-extension://${EXTENSION_ID}/popup.html`);
  const sniffed = await until("the sniffer to record the stream", async () => {
    const all = await cdp.evaluate(probe, "chrome.storage.session.get(null).then(s => Object.values(s).flat())");
    return all.length ? all : null;
  });
  check("the HLS playlist was noticed", sniffed.some((m) => m.kind === "hls" && m.url.endsWith("/hls/master.m3u8")), sniffed.map((m) => `${m.kind}:${m.url.split("/").slice(-2).join("/")}`).join(" "));
  check("stream segments are not listed", !sniffed.some((m) => m.url.endsWith(".ts")));
  const streams = await until("the variant playlist to be folded into its master", async () => {
    const all = await cdp.evaluate(probe, "chrome.storage.session.get(null).then(s => Object.values(s).flat())");
    return all.length === 1 ? all : null;
  }).catch(() => sniffed);
  check("a stream appears once, with its best quality", streams.length === 1 && streams[0].quality === "1080p", streams.map((m) => `${m.url.split("/").pop()} ${m.quality ?? ""}`).join("; "));
  const watchTabId = await cdp.evaluate(probe, `chrome.tabs.query({ url: "${base}/watch" }).then(tabs => tabs[0]?.id)`);

  // The popup, opened for the tab that played the stream.
  const popup = await cdp.open(`chrome-extension://${EXTENSION_ID}/popup.html?tab=${watchTabId}`);
  await cdp.send("Emulation.setDeviceMetricsOverride", { width: 348, height: 560, deviceScaleFactor: 2, mobile: false }, popup.sessionId);
  const status = await until("the popup to report the application", () => cdp.evaluate(popup, "document.getElementById('status').dataset.state !== 'loading' && document.getElementById('status-text').textContent"));
  check("the popup shows Siphon as connected", /Connected/.test(status), status);
  const listed = await until("the popup to list the stream", () => cdp.evaluate(popup, "[...document.querySelectorAll('.item')].map(i => i.querySelector('.item-name').textContent + ' | ' + i.querySelector('.tag').textContent)").then((l) => (l.length ? l : null)));
  check("the popup lists the stream under the page title", listed.length === 1 && listed[0].startsWith("Big Buck Bunny (trailer) | HLS"), listed.join("; "));

  // 3. A picture of the popup, before and after sending.
  const shotDir = join(root, ".e2e");
  mkdirSync(shotDir, { recursive: true });
  const snap = async (name) => {
    const { data } = await cdp.send("Page.captureScreenshot", { format: "png" }, popup.sessionId);
    writeFileSync(join(shotDir, name), Buffer.from(data, "base64"));
  };
  await snap("popup.png");

  await cdp.evaluate(popup, "[...document.querySelectorAll('.item')].find(i => i.querySelector('.tag').textContent === 'HLS' && i.querySelector('.item-name').textContent.startsWith('Big Buck')).querySelector('button').click()");
  const buttonText = await until("the popup to confirm", () => cdp.evaluate(popup, "[...document.querySelectorAll('.item button')].some(b => b.textContent === 'Sent')"));
  check("the popup's Download button hands the stream over", buttonText === true);
  await snap("popup-sent.png");

  const stream = await until("Siphon to assemble the stream", () => library().find((d) => d.protocol === "hls" && d.status.state === "completed"), 60_000);
  check("the stream was saved under the page's title", stream.file_name.startsWith("Big Buck Bunny (trailer)"), stream.file_name);
  check("the best quality was chosen", (stream.note ?? "").startsWith("1080p"), stream.note);
  const streamFile = join(stream.dir, stream.file_name);
  check("all eight segments were stitched", existsSync(streamFile) && statSync(streamFile).size === 8 * 700 * 1024, `${existsSync(streamFile) ? statSync(streamFile).size : 0} bytes`);
  check("popup screenshots saved", existsSync(join(shotDir, "popup.png")), shotDir);

  // 4. The capture rules. Each of these downloads must stay in the browser.
  const setRule = (patch) => cdp.evaluate(probe, `chrome.runtime.sendMessage({ type: "set", patch: ${JSON.stringify(patch)} }).then(r => r.ok)`);
  const inBrowser = (name) => until(`the browser to keep ${name}`, () => existsSync(join(dirs.browser, name)), 20_000).then(() => true, () => false);
  const centreOf = async (target, selector) => cdp.evaluate(target, `(() => { const r = document.querySelector(${JSON.stringify(selector)}).getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 }; })()`);
  await cdp.send("Page.bringToFront", {}, page.sessionId);

  // Alt held: a real mouse press with the modifier, as a person would do it.
  const altLink = await centreOf(page, "[id='kept-by-alt.txt']");
  for (const type of ["mousePressed", "mouseReleased"]) {
    await cdp.send("Input.dispatchMouseEvent", { type, x: altLink.x, y: altLink.y, button: "left", clickCount: 1, modifiers: 1 }, page.sessionId);
  }
  // Chrome's own Alt-click saves the link; where that is not the case the
  // click is repeated plainly, inside the window the modifier opened.
  await sleep(400);
  if (!existsSync(join(dirs.browser, "kept-by-alt.txt"))) await cdp.evaluate(page, "document.getElementById('kept-by-alt.txt').click()");
  check("holding Alt leaves the download to the browser", await inBrowser("kept-by-alt.txt"));

  await setRule({ skipTypes: "txt" });
  await sleep(4_200); // let the Alt window close, so only the type rule is at work
  await cdp.evaluate(page, "document.getElementById('kept-by-type.txt').click()");
  check("a file type left to the browser stays there", await inBrowser("kept-by-type.txt"));

  await setRule({ skipTypes: "", skipHosts: ["127.0.0.1"] });
  await cdp.evaluate(page, "document.getElementById('kept-by-site.bin').click()");
  check("a site left to the browser stays there", await inBrowser("kept-by-site.bin"));
  await setRule({ skipHosts: [] });
  await sleep(500);
  check("none of the three reached Siphon", !library().some((d) => d.file_name.startsWith("kept-by-")), library().map((d) => d.file_name).join(", "));

  // 5. The download button by a video.
  const clip = await cdp.open(`${base}/clip`);
  await cdp.send("Emulation.setDeviceMetricsOverride", { width: 1000, height: 700, deviceScaleFactor: 1, mobile: false }, clip.sessionId);
  await cdp.send("Page.bringToFront", {}, clip.sessionId);
  const placed = await until(
    "the download button to appear by the player",
    () =>
      cdp.evaluate(
        clip,
        `(() => { const b = document.querySelector("siphon-button"); const v = document.getElementById("player"); if (!b || !v) return null;
           const br = b.getBoundingClientRect(), vr = v.getBoundingClientRect(); if (br.width < 40) return null;
           return { x: br.left + 40, y: br.top + br.height / 2 > br.top + 15 ? br.top + 15 : br.top + br.height / 2, top: br.top - vr.top, right: vr.right - br.right, width: br.width }; })()`,
      ),
    20_000,
  ).catch(() => null);
  check("a download button appears on a page with a video", Boolean(placed), placed ? `${Math.round(placed.width)}px wide` : "no button");
  if (placed) {
    check("it sits at the player's top right corner", Math.abs(placed.top - 10) <= 3 && Math.abs(placed.right - 10) <= 3, `top ${Math.round(placed.top)}, right ${Math.round(placed.right)}`);
    const { data } = await cdp.send("Page.captureScreenshot", { format: "png", clip: { x: 0, y: 0, width: 760, height: 520, scale: 2 } }, clip.sessionId);
    writeFileSync(join(shotDir, "video-button.png"), Buffer.from(data, "base64"));
    for (const type of ["mousePressed", "mouseReleased"]) {
      await cdp.send("Input.dispatchMouseEvent", { type, x: placed.x, y: placed.y, button: "left", clickCount: 1 }, clip.sessionId);
    }
    const tour = await until("Siphon to download the video", () => library().find((d) => d.file_name === "Product-Tour.mp4" && d.status.state === "completed"), 45_000).catch(() => null);
    check("clicking it downloads the video through Siphon", Boolean(tour) && tour.total === 6 * 1024 * 1024, tour ? `${tour.total} bytes` : "not in the library");
    check("the page is remembered as its source", tour?.request.referer === `${base}/clip`, tour?.request.referer);
  }

  // 6. Every link of a page, sent to the link picker.
  const galleryPage = await cdp.open(`${base}/gallery/`);
  await until("the gallery page", () => cdp.evaluate(galleryPage, "document.readyState === 'complete'"));
  const galleryTabId = await cdp.evaluate(probe, `chrome.tabs.query({ url: "${base}/gallery/" }).then(tabs => tabs[0]?.id)`);
  const grabbed = await cdp.evaluate(probe, `chrome.runtime.sendMessage({ type: "grab", tabId: ${galleryTabId} })`);
  // 18 photos, their 18 thumbnails, and three links at the bottom.
  check("all links of a page are handed to the link picker", grabbed?.ok === true && grabbed.count === 39, JSON.stringify(grabbed));
} catch (error) {
  check(`aborted: ${error.message}`, false);
} finally {
  // The application was started by the native host, not by this script.
  try {
    const endpoint = JSON.parse(readFileSync(join(dirs.data, "endpoint.json"), "utf8"));
    process.kill(endpoint.pid, "SIGTERM");
  } catch {
    /* it never started */
  }
  cleanup();
}

const failed = checks.filter((c) => !c.ok).length;
console.log(`\n${checks.length - failed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
