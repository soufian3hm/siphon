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
//   6. sends every link of a page to the application's link picker;
//   7. opens a feed of posts with a video each, points at the second one,
//      expects the button on that video, opens its menu, expects that post's
//      qualities under that post's name, picks the smaller one and expects
//      exactly that file to arrive.
//   8. opens a page built the way X marks up its feed, and one whose player
//      sits inside nested shadow trees, and expects the address of the right
//      post (and the right video of it) to be the one Siphon is asked to read.
//
// A stand-in for the video helper answers for these pages, so steps 7 and 8
// need neither the network nor yt-dlp.
//
// Nothing here touches the user's own browser profile or Siphon library.

import { spawn } from "node:child_process";
import { mkdirSync, writeFileSync, readFileSync, existsSync, rmSync, readdirSync, statSync, chmodSync } from "node:fs";
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

// The stand-in video helper: for `…/post/N` it describes that post's two
// files, and notes every call so the test can count them.
const resolver = join(work, "resolver.sh");
writeFileSync(
  resolver,
  `#!/bin/sh
for last; do :; done
echo "$last" >> '${join(work, "resolver-calls.txt")}'
n=$(printf '%s' "$last" | sed -n 's|.*/post/\\([0-9]*\\).*|\\1|p')
# Any other page: the same files, so that its menu has something to show.
if [ -z "$n" ]; then n=1; fi
case "$n" in 1) who='Ada Lovelace'; what='First light over the ridge';; 2) who='Grace Hopper'; what='The harbour at noon';; *) who='Katherine Johnson'; what='Launch day, from the roof';; esac
cat <<JSON
{"title": "$what", "uploader": "$who", "extractor_key": "Fieldnotes", "duration": 30,
 "formats": [
  {"format_id": "http-720", "protocol": "https", "ext": "mp4", "width": 1280, "height": 720, "tbr": 2000, "url": "${base}/media/feed/$n/720.mp4"},
  {"format_id": "http-360", "protocol": "https", "ext": "mp4", "width": 640, "height": 360, "tbr": 600, "url": "${base}/media/feed/$n/360.mp4"}
 ]}
JSON
`,
);
chmodSync(resolver, 0o755);

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
      // One site's name leads to the test server, where a page built like
      // its feed is served (step 8). Only this throwaway browser is affected.
      "--host-resolver-rules=MAP x.com 127.0.0.1",
      "about:blank",
    ],
    { SIPHON_DATA_DIR: dirs.data, SIPHON_RESOLVER: resolver },
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

  // 7. A feed: the button belongs to one post, and its menu to that post.
  const feed = await cdp.open(`${base}/feed`);
  await cdp.send("Emulation.setDeviceMetricsOverride", { width: 1000, height: 1300, deviceScaleFactor: 1, mobile: false }, feed.sessionId);
  await cdp.send("Page.bringToFront", {}, feed.sessionId);
  await cdp.send("DOM.enable", {}, feed.sessionId);
  await until("the feed", () => cdp.evaluate(feed, "document.querySelectorAll('video').length === 3"));
  const boxOf = (id) => cdp.evaluate(feed, `(() => { const r = document.getElementById("${id}").getBoundingClientRect(); return { left: r.left, top: r.top, right: r.right, bottom: r.bottom }; })()`);
  const buttonBox = () => cdp.evaluate(feed, `(() => { const b = document.querySelector("siphon-button"); if (!b) return null; const r = b.getBoundingClientRect(); return r.width < 40 ? null : { left: r.left, top: r.top, right: r.right, bottom: r.bottom }; })()`);
  const pointAt = async (id) => {
    const box = await boxOf(id);
    await cdp.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: box.left + 120, y: box.top + 200 }, feed.sessionId);
    return box;
  };
  /** The button lives in a closed shadow tree; the debug protocol sees into it. */
  const inButton = async (selector) => {
    const { root: documentNode } = await cdp.send("DOM.getDocument", { depth: 0 }, feed.sessionId);
    const { nodeId: hostId } = await cdp.send("DOM.querySelector", { nodeId: documentNode.nodeId, selector: "siphon-button" }, feed.sessionId);
    if (!hostId) return [];
    const { node } = await cdp.send("DOM.describeNode", { nodeId: hostId, depth: 1, pierce: true }, feed.sessionId);
    const shadowBackend = node.shadowRoots?.[0]?.backendNodeId;
    if (!shadowBackend) return [];
    const { nodeIds: [shadowId] } = await cdp.send("DOM.pushNodesByBackendIdsToFrontend", { backendNodeIds: [shadowBackend] }, feed.sessionId);
    const { nodeIds } = await cdp.send("DOM.querySelectorAll", { nodeId: shadowId, selector }, feed.sessionId);
    const found = [];
    for (const nodeId of nodeIds) {
      const { outerHTML } = await cdp.send("DOM.getOuterHTML", { nodeId }, feed.sessionId);
      const box = await cdp.send("DOM.getBoxModel", { nodeId }, feed.sessionId).then((r) => r.model.content, () => null);
      found.push({ text: outerHTML.replace(/<[^>]+>/g, " ").replace(/&amp;/g, "&").replace(/\s+/g, " ").trim(), x: box ? (box[0] + box[2]) / 2 : 0, y: box ? (box[1] + box[5]) / 2 : 0 });
    }
    return found;
  };
  const click = async (x, y) => {
    for (const type of ["mousePressed", "mouseReleased"]) {
      await cdp.send("Input.dispatchMouseEvent", { type, x, y, button: "left", clickCount: 1 }, feed.sessionId);
    }
  };
  const callsFor = (n) => (existsSync(join(work, "resolver-calls.txt")) ? readFileSync(join(work, "resolver-calls.txt"), "utf8").split("\n").filter((line) => line.endsWith(`/post/${n}`)).length : 0);

  const second = await pointAt("video-2");
  const onSecond = await until("the button on the second post's video", async () => {
    const box = await buttonBox();
    return box && Math.abs(box.top - second.top - 10) <= 3 && Math.abs(second.right - box.right - 10) <= 3 ? box : null;
  }, 20_000).catch(() => null);
  check("on a feed the button sits on the video under the pointer", Boolean(onSecond), onSecond ? `top ${Math.round(onSecond.top - second.top)}, right ${Math.round(second.right - onSecond.right)} of the second video` : `button ${JSON.stringify(await buttonBox())}; videos ${JSON.stringify([await boxOf("video-1"), await boxOf("video-2"), await boxOf("video-3")])}`);
  if (onSecond) {
    const [pill] = await inButton(".main");
    await click(pill.x, pill.y);
    const rows = await until("the qualities of that post", async () => {
      const found = await inButton(".row");
      return found.length > 0 ? found : null;
    }, 30_000).catch(() => []);
    const [title] = await inButton(".title");
    const [sub] = await inButton(".sub");
    check("its menu is named after that post", title?.text === "Grace Hopper - The harbour at noon", `${title?.text}  /  ${sub?.text}`);
    check("the menu lists that post's qualities with their sizes", rows.length === 2 && /^720p MP4 \u2248 7\.2 MB$/.test(rows[0]?.text ?? "") && /^360p MP4 \u2248 2\.1 MB$/.test(rows[1]?.text ?? ""), rows.map((row) => row.text).join(" | "));
    const { data } = await cdp.send("Page.captureScreenshot", { format: "png", clip: { x: 150, y: 360, width: 700, height: 440, scale: 2 } }, feed.sessionId);
    writeFileSync(join(shotDir, "quality-menu.png"), Buffer.from(data, "base64"));
    if (rows.length === 2) {
      await click(rows[1].x, rows[1].y);
      const post = await until("Siphon to download the chosen quality", () => library().find((d) => d.file_name === "Grace Hopper - The harbour at noon.mp4" && d.status.state === "completed"), 45_000).catch(() => null);
      check("the quality picked by the player is the file that arrives", post?.total === 2 * 300 * 1024 && post?.variant === "360", post ? `${post.total} bytes, quality ${post.variant}` : "not in the library");
      check("it is saved under the post's author and text", Boolean(post), post?.file_name);
      check("the download is the post, not the feed", post?.request.url === `${base}/post/2` && post?.request.page === true, post?.request.url);
      check("the page was read once for the menu and the download", callsFor(2) === 1, `${callsFor(2)} readings`);
      check("no other post was read or downloaded", callsFor(1) === 0 && callsFor(3) === 0 && library().filter((d) => /harbour|ridge|Launch/.test(d.file_name)).length === 1);
    }
    // The menu is gone and the button follows the pointer to another post.
    const third = await pointAt("video-3");
    const onThird = await until("the button on the third post's video", async () => {
      const box = await buttonBox();
      return box && Math.abs(box.top - third.top - 10) <= 3 ? box : null;
    }, 15_000).catch(() => null);
    check("the button moves with the pointer to another post", Boolean(onThird), JSON.stringify(await buttonBox()));
  }

  // 8. Where a site keeps a post's address. The pages are stand-ins; what
  // is checked is the address the extension asks Siphon to read for the
  // video pointed at.
  const allCalls = () => (existsSync(join(work, "resolver-calls.txt")) ? readFileSync(join(work, "resolver-calls.txt"), "utf8") : "");
  const askedFor = async (pageUrl, locate, label) => {
    const page = await cdp.open(pageUrl);
    await cdp.send("Emulation.setDeviceMetricsOverride", { width: 1000, height: 1400, deviceScaleFactor: 1, mobile: false }, page.sessionId);
    await cdp.send("Page.bringToFront", {}, page.sessionId);
    await cdp.send("DOM.enable", {}, page.sessionId);
    const spot = await until(`the player of ${label}`, () => cdp.evaluate(page, `(() => { const el = ${locate}; if (!el) return null; const r = el.getBoundingClientRect(); return r.width > 200 ? { x: r.left + 100, y: r.top + 150, top: r.top } : null; })()`), 15_000).catch(() => null);
    if (!spot) return { button: false, asked: "the player never appeared" };
    await cdp.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: spot.x, y: spot.y }, page.sessionId);
    const pierce = async (selector) => {
      const { root: documentNode } = await cdp.send("DOM.getDocument", { depth: 0 }, page.sessionId);
      const { nodeId } = await cdp.send("DOM.querySelector", { nodeId: documentNode.nodeId, selector: "siphon-button" }, page.sessionId);
      if (!nodeId) return [];
      const { node } = await cdp.send("DOM.describeNode", { nodeId, depth: 1, pierce: true }, page.sessionId);
      const backend = node.shadowRoots?.[0]?.backendNodeId;
      if (!backend) return [];
      const { nodeIds: [shadowId] } = await cdp.send("DOM.pushNodesByBackendIdsToFrontend", { backendNodeIds: [backend] }, page.sessionId);
      const { nodeIds } = await cdp.send("DOM.querySelectorAll", { nodeId: shadowId, selector }, page.sessionId);
      const found = [];
      for (const id of nodeIds) {
        const box = await cdp.send("DOM.getBoxModel", { nodeId: id }, page.sessionId).then((r) => r.model.content, () => null);
        if (box) found.push({ x: (box[0] + box[2]) / 2, y: (box[1] + box[5]) / 2, top: box[1] });
      }
      return found;
    };
    const pill = await until(`the button for ${label}`, async () => {
      const [found] = await pierce(".main");
      return found && Math.abs(found.top - spot.top - 11) <= 6 ? found : null;
    }, 6_000).catch(() => null);
    if (!pill) return { button: false, asked: "" };
    const before = allCalls().length;
    for (const type of ["mousePressed", "mouseReleased"]) {
      await cdp.send("Input.dispatchMouseEvent", { type, x: pill.x, y: pill.y, button: "left", clickCount: 1 }, page.sessionId);
    }
    await until(`the menu for ${label}`, async () => (await pierce(".row")).length > 0, 30_000).catch(() => null);
    return { button: true, asked: allCalls().slice(before).trim().split("\n").pop() || "(nothing new was read)" };
  };

  const xBase = `http://x.com:${SERVER_PORT}`;
  const single = await askedFor(`${xBase}/replica/x`, `document.getElementById("x-1")`, "a post with one video");
  check("a post in a feed is read by its own address", single.asked === `${xBase}/ada/status/1790000000000000001`, single.asked);
  const twin = await askedFor(`${xBase}/replica/x`, `document.getElementById("x-2b")`, "the second video of a post");
  check("the second video of a post is asked for by number", twin.asked === `${xBase}/grace/status/1790000000000000002/video/2`, twin.asked);
  const advert = await askedFor(`${xBase}/replica/x`, `document.getElementById("x-ad")`, "a video with no post behind it");
  check("a video with no post of its own gets no button", advert.button === false, advert.asked);
  const nested = await askedFor(`${base}/replica/shadow`, `document.getElementById("shadow-player")`, "a player inside shadow trees");
  check("a player two shadow trees deep is found, and its post with it", nested.asked === `${base}/post/9`, nested.asked);
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
