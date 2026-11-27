// Siphon browser extension: service worker.
//
// Four jobs:
//   1. Take over downloads the user starts and hand them to the application,
//      within the limits the user set (sites, sizes and types left alone,
//      Alt held while clicking).
//   2. Notice media streams (HLS playlists, DASH manifests, direct video and
//      audio) as pages request them, list them per tab for the popup, and
//      tell the page so it can show a download button by the player.
//   3. Offer "Download with Siphon" in the context menu.
//   4. Collect every link of a page for the application's link picker.
//
// The application is reached through a native-messaging host. Nothing is
// taken over unless that host answers: if Siphon is not installed, the
// browser downloads exactly as it always did.

const HOST = "app.siphon.host";
const REQUEST_TIMEOUT_MS = 25_000;
/** Reading a video page takes the application a few seconds, sometimes many. */
const DESCRIBE_TIMEOUT_MS = 110_000;
const MAX_MEDIA_PER_TAB = 40;

const DEFAULTS = Object.freeze({
  enabled: true,
  sniffMedia: true,
  /** Downloads smaller than this stay in the browser. 0 takes everything. */
  minSizeKB: 0,
  /** Hosts whose downloads stay in the browser. */
  skipHosts: [],
  /** Extensions that stay in the browser, e.g. "pdf jpg png". */
  skipTypes: "",
  /** Show a download button by players on pages where media was found. */
  videoButton: true,
});

/** Tabs where the next download was asked to stay in the browser (Alt held). */
const bypassTabs = new Map();
const BYPASS_WINDOW_MS = 4_000;

function matchesHost(host, skip) {
  return host === skip || host.endsWith(`.${skip}`);
}

function extensionOf(name) {
  const match = /\.([a-z0-9]{1,10})$/i.exec((name ?? "").split(/[?#]/)[0]);
  return match ? match[1].toLowerCase() : "";
}

// ---------------------------------------------------------------- settings --

async function settings() {
  const stored = await chrome.storage.local.get("settings");
  return { ...DEFAULTS, ...(stored.settings ?? {}) };
}

// ------------------------------------------------------------- native host --

/** @type {chrome.runtime.Port | null} */
let port = null;
let nextId = 1;
/** @type {Map<number, {resolve: (v: any) => void, reject: (e: Error) => void, timer: number}>} */
const pending = new Map();
let lastHostError = "";

function failAll(reason) {
  for (const { reject, timer } of pending.values()) {
    clearTimeout(timer);
    reject(new Error(reason));
  }
  pending.clear();
}

function connect() {
  if (port) return port;
  try {
    port = chrome.runtime.connectNative(HOST);
  } catch (error) {
    lastHostError = String(error?.message ?? error);
    port = null;
    throw error;
  }
  port.onMessage.addListener((message) => {
    const waiter = pending.get(message?.id);
    if (!waiter) return;
    pending.delete(message.id);
    clearTimeout(waiter.timer);
    waiter.resolve(message);
  });
  port.onDisconnect.addListener(() => {
    lastHostError = chrome.runtime.lastError?.message ?? "The Siphon helper stopped.";
    port = null;
    failAll(lastHostError);
  });
  return port;
}

/** Sends one request to the host and resolves with its reply. */
function ask(message, patience = REQUEST_TIMEOUT_MS) {
  return new Promise((resolve, reject) => {
    let channel;
    try {
      channel = connect();
    } catch (error) {
      reject(error instanceof Error ? error : new Error(String(error)));
      return;
    }
    const id = nextId++;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error("Siphon did not answer in time."));
    }, patience);
    pending.set(id, { resolve, reject, timer });
    try {
      channel.postMessage({ ...message, id });
    } catch (error) {
      pending.delete(id);
      clearTimeout(timer);
      reject(error instanceof Error ? error : new Error(String(error)));
    }
  });
}

/**
 * Whether the helper is installed, and whether the application is running.
 * @returns {Promise<{installed: boolean, running: boolean, version?: string, error?: string}>}
 */
async function hostStatus() {
  try {
    const reply = await ask({ type: "ping" });
    lastHostError = "";
    if (reply.running) followUpdate(reply.version);
    return { installed: true, running: Boolean(reply.running), version: reply.version };
  } catch (error) {
    return { installed: false, running: false, error: lastHostError || String(error?.message ?? error) };
  }
}

/**
 * Siphon keeps the folder this extension was loaded from up to date. When the
 * application is newer than the copy the browser has in memory, the extension
 * loads itself again from that folder: once per version, so a copy loaded
 * from somewhere Siphon does not update cannot loop.
 */
async function followUpdate(appVersion) {
  const numbers = (version) => String(version ?? "").split(".").map((part) => Number.parseInt(part, 10) || 0);
  const [app, mine] = [numbers(appVersion), numbers(chrome.runtime.getManifest().version)];
  let newer = false;
  for (let i = 0; i < Math.max(app.length, mine.length); i++) {
    if ((app[i] ?? 0) !== (mine[i] ?? 0)) {
      newer = (app[i] ?? 0) > (mine[i] ?? 0);
      break;
    }
  }
  if (!newer) return;
  const stored = await chrome.storage.local.get("reloadedFor");
  if (stored.reloadedFor === appVersion) return;
  await chrome.storage.local.set({ reloadedFor: appVersion });
  chrome.runtime.reload();
}

// ---------------------------------------------------------------- captures --

function isFetchable(url) {
  return /^(https?|ftp):\/\//i.test(url ?? "");
}

function hostOf(url) {
  try {
    return new URL(url).hostname.toLowerCase();
  } catch {
    return "";
  }
}

function baseName(path) {
  return (path ?? "").split(/[\\/]/).pop() ?? "";
}

/**
 * A page whose video the application resolves itself from the page address
 * (a YouTube watch page), as opposed to a file or stream address.
 */
function isVideoPage(url) {
  try {
    const parsed = new URL(url);
    const host = parsed.hostname.replace(/^(www|m|music)\./, "");
    if (host === "youtu.be") return parsed.pathname.length > 1;
    if (host !== "youtube.com") return false;
    return (parsed.pathname === "/watch" && Boolean(parsed.searchParams.get("v"))) || parsed.pathname.startsWith("/shorts/");
  } catch {
    return false;
  }
}

/** YouTube is read logged out, always: its public videos need no account. */
function isYouTube(url) {
  return /^(youtube\.com|youtube-nocookie\.com|youtu\.be)$/.test(hostOf(url).replace(/^(www|m|music)\./, ""));
}

/**
 * The request for a page with a video on it. The browser's cookies for the
 * page go along (never YouTube's): the application reads the page logged out
 * and only falls back on them when the site refuses that.
 */
async function pageRequest(url, referer) {
  const headers = [["User-Agent", navigator.userAgent]];
  const cookie = isYouTube(url) ? "" : await cookieHeader(url);
  if (cookie) headers.push(["Cookie", cookie]);
  return { url, referer: referer || undefined, headers, page: true };
}

/** What Siphon found behind a page, kept briefly: the menu is opened twice. */
const described = new Map();

/** Siphon is started for this only when `launch` says the user clicked. */
async function describePage(url, referer, launch) {
  const kept = described.get(url);
  if (kept && Date.now() - kept.at < 180_000) return kept.page;
  const reply = await ask({ type: "describe", request: await pageRequest(url, referer), launch: launch === true }, DESCRIBE_TIMEOUT_MS);
  if (!reply.ok || !reply.page) throw new Error(reply.error || "Siphon could not read this page.");
  described.set(url, { at: Date.now(), page: reply.page });
  if (described.size > 60) described.delete(described.keys().next().value);
  return reply.page;
}

/** The cookies the browser would send to `url`, as a `Cookie` header value. */
async function cookieHeader(url) {
  if (!/^https?:/i.test(url)) return "";
  try {
    const cookies = await chrome.cookies.getAll({ url });
    return cookies.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch {
    return "";
  }
}

async function tabInfo(tabId) {
  if (typeof tabId !== "number" || tabId < 0) return {};
  try {
    const tab = await chrome.tabs.get(tabId);
    return { url: tab.url, title: tab.title };
  } catch {
    return {};
  }
}

/**
 * Builds the message the application understands.
 * @param {{url: string, referer?: string, name?: string, size?: number, mime?: string, title?: string, media?: boolean, page?: boolean, variant?: string}} what
 */
async function buildCapture(what) {
  // A page with a video on it: said so by the page script, which saw the
  // player, or plain from the address.
  const page = Boolean(what.page) || isVideoPage(what.url);
  let request;
  if (page) {
    request = await pageRequest(what.url, what.referer);
  } else {
    const headers = [["User-Agent", navigator.userAgent]];
    const cookie = await cookieHeader(what.url);
    if (cookie) headers.push(["Cookie", cookie]);
    request = { url: what.url, referer: what.referer || undefined, headers };
  }
  return {
    request,
    suggested_name: what.name || undefined,
    size: Number.isFinite(what.size) && what.size > 0 ? Math.round(what.size) : undefined,
    mime: what.mime || undefined,
    page_title: what.title || undefined,
    media: Boolean(what.media) || page,
    // The quality chosen by the player, when one was.
    variant: what.variant || undefined,
  };
}

/** Sends a capture. Resolves true when the application accepted it. */
async function sendCapture(what) {
  const capture = await buildCapture(what);
  const reply = await ask({ type: "capture", capture });
  if (!reply.ok) throw new Error(reply.error || "Siphon refused the download.");
  return true;
}

// --------------------------------------------------- download interception --

/** URLs the extension itself handed back to the browser; never re-captured. */
const bypass = new Set();
/** URLs recently requested with a body. Replaying those as GET would break. */
const postedRecently = new Map();

chrome.webRequest.onBeforeRequest.addListener(
  (details) => {
    if (details.method !== "GET" && details.method !== "HEAD") {
      postedRecently.set(details.url, Date.now());
      // Small and self-pruning.
      if (postedRecently.size > 200) {
        const cutoff = Date.now() - 120_000;
        for (const [url, at] of postedRecently) if (at < cutoff) postedRecently.delete(url);
      }
    }
  },
  { urls: ["<all_urls>"], types: ["main_frame", "sub_frame", "other", "xmlhttprequest"] },
);

async function shouldTake(item) {
  const config = await settings();
  if (!config.enabled) return false;
  if (item.byExtensionId === chrome.runtime.id) return false;
  const url = item.finalUrl || item.url;
  if (!isFetchable(url)) return false; // blob:, data:, file:, extension pages
  if (bypass.has(url) || bypass.has(item.url)) return false;
  for (const candidate of [url, item.url]) {
    const posted = postedRecently.get(candidate);
    if (posted && Date.now() - posted < 120_000) return false;
  }
  const size = item.fileSize > 0 ? item.fileSize : item.totalBytes;
  if (config.minSizeKB > 0 && size > 0 && size < config.minSizeKB * 1024) return false;
  // A site left alone: by the file's own host, or by the page it was on.
  const hosts = [hostOf(url), hostOf(item.referrer)].filter(Boolean);
  if (config.skipHosts.some((skip) => hosts.some((host) => matchesHost(host, skip)))) return false;
  const skipped = config.skipTypes.toLowerCase().split(/[\s,;.]+/).filter(Boolean);
  const type = extensionOf(baseName(item.filename)) || extensionOf(url);
  if (type && skipped.includes(type)) return false;
  // Alt was held for a click in the active tab a moment ago.
  const tab = await activeTab();
  const until = tab?.id !== undefined ? bypassTabs.get(tab.id) : undefined;
  if (until) {
    bypassTabs.delete(tab.id);
    if (Date.now() < until) return false;
  }
  return true;
}

async function takeOver(item) {
  if (!(await shouldTake(item))) return false;
  // Ask before touching the browser's download: if the helper is missing
  // there is nothing to hand over to.
  const status = await hostStatus();
  if (!status.installed) return false;

  const url = item.finalUrl || item.url;
  try {
    await chrome.downloads.cancel(item.id);
  } catch {
    // Already finished or gone: too late to take it.
    return false;
  }

  try {
    const tab = await activeTab();
    await sendCapture({
      url,
      referer: item.referrer || tab?.url,
      name: baseName(item.filename),
      size: item.fileSize > 0 ? item.fileSize : item.totalBytes,
      mime: item.mime,
      title: tab?.title,
    });
    // Keep the browser's own list free of the cancelled entry.
    chrome.downloads.erase({ id: item.id }).catch(() => {});
    await flashBadge("↓", "#4C8DFF");
    return true;
  } catch (error) {
    // The application could not take it. Give the download back.
    lastHostError = String(error?.message ?? error);
    bypass.add(url);
    setTimeout(() => bypass.delete(url), 60_000);
    chrome.downloads.erase({ id: item.id }).catch(() => {});
    chrome.downloads.download({ url }).catch(() => {});
    await flashBadge("!", "#F2555A");
    return false;
  }
}

async function activeTab() {
  try {
    const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
    return tab ?? null;
  } catch {
    return null;
  }
}

chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
  takeOver(item)
    .catch(() => false)
    .finally(() => {
      // Chrome waits for this either way; for a download we took over it is
      // a no-op, for one we left alone it lets it proceed under its own name.
      try {
        suggest();
      } catch {
        /* the download is already gone */
      }
    });
  return true; // `suggest` is called asynchronously
});

// ------------------------------------------------------------ media sniffer --

const MANIFEST_TYPES = new Map([
  ["application/vnd.apple.mpegurl", "hls"],
  ["application/x-mpegurl", "hls"],
  ["audio/mpegurl", "hls"],
  ["audio/x-mpegurl", "hls"],
  ["application/dash+xml", "dash"],
]);

function classify(url, contentType, length) {
  let path = "";
  try {
    path = new URL(url).pathname.toLowerCase();
  } catch {
    return null;
  }
  const type = (contentType ?? "").split(";")[0].trim().toLowerCase();
  if (path.endsWith(".m3u8") || MANIFEST_TYPES.get(type) === "hls") return "hls";
  if (path.endsWith(".mpd") || MANIFEST_TYPES.get(type) === "dash") return "dash";
  // Stream segments are pieces, not something to offer on their own.
  if (/\.(ts|m4s|cmfv|cmfa|aac|vtt|webvtt|key)$/.test(path)) return null;
  // A whole media file, not a thumbnail-sized clip.
  if (length >= 1024 * 1024) {
    if (type.startsWith("video/")) return "video";
    if (type.startsWith("audio/")) return "audio";
  }
  return null;
}

async function mediaFor(tabId) {
  const key = `media:${tabId}`;
  const stored = await chrome.storage.session.get(key);
  return stored[key] ?? [];
}

/**
 * Reads an HLS playlist to learn what it is: a master playlist (with the
 * qualities it offers), a finished stream, or a live one.
 */
async function describePlaylist(url) {
  try {
    const response = await fetch(url, { credentials: "include" });
    if (!response.ok) return null;
    const text = (await response.text()).slice(0, 512 * 1024);
    if (!text.trimStart().startsWith("#EXTM3U")) return null;
    const lines = text.split(/\r?\n/);
    const variants = [];
    const qualities = new Set();
    let height = 0;
    for (let i = 0; i < lines.length; i++) {
      // Sound and subtitles delivered as playlists of their own are part of
      // the same stream, not streams to offer.
      if (lines[i].startsWith("#EXT-X-MEDIA") || lines[i].startsWith("#EXT-X-I-FRAME-STREAM-INF")) {
        const part = /URI="([^"]+)"/.exec(lines[i])?.[1];
        try {
          if (part) variants.push(new URL(part, url).href);
        } catch {
          /* not an address */
        }
        continue;
      }
      if (!lines[i].startsWith("#EXT-X-STREAM-INF")) continue;
      const target = lines.slice(i + 1).find((line) => line.trim() && !line.startsWith("#"));
      if (!target) continue;
      try {
        variants.push(new URL(target.trim(), url).href);
      } catch {
        continue;
      }
      const size = /RESOLUTION=(\d+)x(\d+)/.exec(lines[i]);
      if (size) {
        height = Math.max(height, Number(size[2]));
        // A quality is named by the picture's short side.
        qualities.add(Math.min(Number(size[1]), Number(size[2])));
      }
    }
    const master = variants.length > 0;
    return { variants, height, qualities: [...qualities].sort((a, b) => b - a), live: !master && !text.includes("#EXT-X-ENDLIST") };
  } catch {
    return null;
  }
}

/** Writes to one tab's list happen one after another, never interleaved. */
const listQueues = new Map();

function rememberMedia(tabId, entry) {
  const run = async () => {
    const key = `media:${tabId}`;
    if (entry.kind === "hls") {
      const info = await describePlaylist(entry.url);
      if (info) {
        entry.variants = info.variants;
        entry.qualities = info.qualities;
        entry.quality = info.qualities[0] ? `${info.qualities[0]}p` : info.height > 0 ? `${info.height}p` : "";
        entry.live = info.live;
      }
    }
    let list = await mediaFor(tabId);
    if (list.some((existing) => existing.url === entry.url)) return;
    list.unshift(entry);
    // A master playlist stands for all of its qualities: the individual
    // variant playlists a player then requests are the same stream again.
    const covered = new Set(list.flatMap((item) => item.variants ?? []));
    list = list.filter((item) => !covered.has(item.url));
    list.length = Math.min(list.length, MAX_MEDIA_PER_TAB);
    await chrome.storage.session.set({ [key]: list });
    await showCount(tabId, list.length);
    // The page shows its download button as soon as there is something for it.
    chrome.tabs.sendMessage(tabId, { type: "siphon-media", count: list.length }).catch(() => {});
  };
  const queued = (listQueues.get(tabId) ?? Promise.resolve()).then(run, run);
  listQueues.set(tabId, queued);
  return queued;
}

async function showCount(tabId, count) {
  try {
    await chrome.action.setBadgeBackgroundColor({ tabId, color: "#2B5CC4" });
    await chrome.action.setBadgeTextColor?.({ tabId, color: "#FFFFFF" });
    await chrome.action.setBadgeText({ tabId, text: count > 0 ? String(count) : "" });
  } catch {
    /* the tab closed meanwhile */
  }
}

async function flashBadge(text, color) {
  const tab = await activeTab();
  if (!tab?.id) return;
  try {
    await chrome.action.setBadgeBackgroundColor({ tabId: tab.id, color });
    await chrome.action.setBadgeText({ tabId: tab.id, text });
    setTimeout(async () => showCount(tab.id, (await mediaFor(tab.id)).length), 2500);
  } catch {
    /* ignore */
  }
}

chrome.webRequest.onResponseStarted.addListener(
  (details) => {
    if (details.tabId < 0 || details.statusCode < 200 || details.statusCode >= 300) return;
    const header = (name) => details.responseHeaders?.find((h) => h.name.toLowerCase() === name)?.value;
    const range = header("content-range")?.match(/\/(\d+)\s*$/);
    const length = Number(range?.[1] ?? header("content-length") ?? 0);
    const kind = classify(details.url, header("content-type"), length);
    if (!kind) return;
    settings().then((config) => {
      if (!config.sniffMedia) return;
      rememberMedia(details.tabId, {
        url: details.url,
        kind,
        mime: (header("content-type") ?? "").split(";")[0].trim(),
        size: kind === "video" || kind === "audio" ? length : 0,
        initiator: details.initiator ?? "",
        seenAt: Date.now(),
      });
    });
  },
  { urls: ["<all_urls>"], types: ["xmlhttprequest", "media", "other", "object"] },
  ["responseHeaders"],
);

// A new page in the tab starts a new list.
chrome.tabs.onUpdated.addListener((tabId, change) => {
  if (change.url || change.status === "loading") {
    chrome.storage.session.remove(`media:${tabId}`).then(() => showCount(tabId, 0));
  }
});
chrome.tabs.onRemoved.addListener((tabId) => {
  listQueues.delete(tabId);
  chrome.storage.session.remove(`media:${tabId}`);
});

// ------------------------------------------------------------ context menu --

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({ id: "siphon-link", title: "Download link with Siphon", contexts: ["link"] });
    chrome.contextMenus.create({ id: "siphon-media", title: "Download with Siphon", contexts: ["image", "video", "audio"] });
    chrome.contextMenus.create({ id: "siphon-selection", title: "Download selected links with Siphon\u2026", contexts: ["selection"] });
    chrome.contextMenus.create({ id: "siphon-all", title: "Download all links with Siphon\u2026", contexts: ["page", "frame"] });
  });
});

/**
 * Runs in the page: every link and embedded file it shows. Kept free of
 * outside references, because it is serialised into the tab.
 */
function collectLinks(selectionOnly) {
  const links = [];
  const seen = new Set();
  const pageLike = /\.(html?|xhtml|shtml|php\d?|aspx?|jspx?|cfm|cgi)$/i;
  const selection = getSelection();
  const wanted = (node) => !selectionOnly || (selection && selection.rangeCount > 0 && selection.containsNode(node, true));
  const push = (address, text, kind) => {
    try {
      const url = new URL(address, document.baseURI);
      if (!/^(https?|ftp):$/.test(url.protocol)) return;
      url.hash = "";
      if (seen.has(url.href)) return;
      seen.add(url.href);
      links.push({ url: url.href, text: (text ?? "").replace(/\s+/g, " ").trim().slice(0, 160), kind });
    } catch {
      /* not an address */
    }
  };
  for (const anchor of document.querySelectorAll("a[href], area[href]")) {
    if (!wanted(anchor)) continue;
    let kind = "page";
    try {
      const name = new URL(anchor.href).pathname.split("/").pop() ?? "";
      if (anchor.hasAttribute("download") || (/\.[a-z0-9]{1,10}$/i.test(name) && !pageLike.test(name))) kind = "file";
    } catch {
      /* stays a page */
    }
    push(anchor.href, anchor.textContent?.trim() || anchor.title || anchor.querySelector("img[alt]")?.alt, kind);
  }
  if (!selectionOnly) {
    for (const element of document.querySelectorAll("img, video, audio, source, embed")) {
      const source = element.currentSrc || element.src || element.dataset?.src;
      if (source && !/^(data|blob):/i.test(source)) push(source, element.alt || element.title, "embedded");
    }
  }
  return { page_url: location.href, page_title: document.title, links: links.slice(0, 5000) };
}

/** Collects a tab's links and hands them to the application's link picker. */
async function grabLinks(tabId, selectionOnly) {
  const [result] = await chrome.scripting.executeScript({ target: { tabId }, func: collectLinks, args: [Boolean(selectionOnly)] });
  const batch = result?.result;
  if (!batch || batch.links.length === 0) throw new Error(selectionOnly ? "The selection contains no links." : "This page has no links.");
  batch.headers = [["User-Agent", navigator.userAgent]];
  const cookie = await cookieHeader(batch.page_url);
  if (cookie) batch.headers.push(["Cookie", cookie]);
  const reply = await ask({ type: "links", links: batch });
  if (!reply.ok) throw new Error(reply.error || "Siphon refused the links.");
  return batch.links.length;
}

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "siphon-all" || info.menuItemId === "siphon-selection") {
    try {
      await grabLinks(tab.id, info.menuItemId === "siphon-selection");
      await flashBadge("\u2193", "#4C8DFF");
    } catch (error) {
      lastHostError = String(error?.message ?? error);
      await flashBadge("!", "#F2555A");
    }
    return;
  }
  const url = info.menuItemId === "siphon-link" ? info.linkUrl : info.srcUrl;
  if (!isFetchable(url)) {
    await flashBadge("!", "#F2555A");
    return;
  }
  try {
    await sendCapture({ url, referer: info.pageUrl || tab?.url, title: tab?.title });
    await flashBadge("↓", "#4C8DFF");
  } catch (error) {
    lastHostError = String(error?.message ?? error);
    await flashBadge("!", "#F2555A");
  }
});

// -------------------------------------------------------------------- popup --

chrome.runtime.onMessage.addListener((message, sender, respond) => {
  (async () => {
    switch (message?.type) {
      // From the page script: Alt was held, so the next download stays put.
      case "bypass-next": {
        if (sender.tab?.id !== undefined) bypassTabs.set(sender.tab.id, Date.now() + BYPASS_WINDOW_MS);
        return {};
      }
      // From the page script: what was found on its tab.
      case "page-media": {
        const config = await settings();
        const media = sender.tab?.id !== undefined ? await mediaFor(sender.tab.id) : [];
        return { media, videoButton: config.sniffMedia && config.videoButton };
      }
      // From the page script: what does the page of this video offer?
      case "describe": {
        if (!/^https?:\/\//i.test(message.url ?? "")) throw new Error("That is not a page.");
        return { page: await describePage(message.url, message.referer, message.launch) };
      }
      case "grab": {
        const tabId = Number.isInteger(message.tabId) ? message.tabId : (await activeTab())?.id;
        if (tabId === undefined) throw new Error("There is no page to read.");
        return { count: await grabLinks(tabId, false) };
      }
      case "state": {
        // The popup normally describes the active tab; it can be opened for a
        // specific one with `popup.html?tab=<id>`.
        const wanted = Number.isInteger(message.tabId) ? chrome.tabs.get(message.tabId).catch(() => null) : activeTab();
        const [config, status, tab] = await Promise.all([settings(), hostStatus(), wanted]);
        let media = tab?.id ? await mediaFor(tab.id) : [];
        // On a video page, the page is what there is to download.
        if (tab?.url && isVideoPage(tab.url)) media = [{ url: tab.url, kind: "page", mime: "", size: 0 }];
        return { settings: config, host: status, media, tab: tab ? { id: tab.id, url: tab.url, title: tab.title } : null };
      }
      case "set": {
        const next = { ...(await settings()), ...message.patch };
        await chrome.storage.local.set({ settings: next });
        return { settings: next };
      }
      case "download": {
        // The popup names the tab; the page script is in it.
        const tab = await tabInfo(Number.isInteger(message.tabId) ? message.tabId : sender.tab?.id);
        // A stream has no file name of its own; the page title is the best one.
        const isStream = message.kind === "hls" || message.kind === "dash";
        const page = Boolean(message.page) || message.kind === "page";
        // What the page script made of the post, else the tab's title.
        const clean = (name) => (name ?? "").replace(/[\\/:*?"<>|]+/g, " ").replace(/\s+/g, " ").trim().slice(0, 120);
        const [given, tabTitle] = [clean(message.name), clean(tab.title)];
        let name;
        if (page) name = undefined; // named by the application once it has read the page
        else if (isStream) name = given || tabTitle ? `${given || tabTitle}.mp4` : undefined;
        else if (given) name = /\.[a-z0-9]{2,5}$/i.test(given) ? given : `${given}.${extensionOf(message.url) || "mp4"}`;
        await sendCapture({
          url: message.url,
          page,
          variant: message.variant,
          // A player inside another site's page answers to that page.
          referer: page ? message.referer : tab.url,
          name,
          size: message.size,
          mime: message.mime,
          title: tab.title,
          media: true,
        });
        return { ok: true };
      }
      case "raise": {
        const reply = await ask({ type: "raise" });
        if (!reply.ok) throw new Error(reply.error || "Siphon could not be opened.");
        return { ok: true };
      }
      default:
        throw new Error("Unknown request.");
    }
  })().then(
    (result) => respond({ ok: true, ...result }),
    (error) => respond({ ok: false, error: String(error?.message ?? error) }),
  );
  return true; // respond asynchronously
});
