// Siphon browser extension: popup.

const $ = (id) => document.getElementById(id);

const statusLine = $("status");
const statusText = $("status-text");
const openButton = $("open");
const enabledSwitch = $("enabled");
const sniffSwitch = $("sniff");
const siteRow = $("site-row");
const siteSwitch = $("site");
const videoSwitch = $("video-button");
const minSize = $("min-size");
const skipTypes = $("skip-types");
const grabButton = $("grab");
const grabNote = $("grab-note");
/** The page's host and the list of hosts left to the browser. */
let site = "";
let skipHosts = [];
const notice = $("notice");
const list = $("media-list");
const emptyNote = $("media-empty");
const countLabel = $("media-count");

/** Sends a request to the service worker and unwraps its reply. */
async function request(message) {
  const reply = await chrome.runtime.sendMessage(message);
  if (!reply?.ok) throw new Error(reply?.error || "The extension did not answer.");
  return reply;
}

function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes <= 0) return "";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = value >= 100 || unit === 0 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(digits)} ${units[unit]}`;
}

const KIND_LABEL = { hls: "HLS", dash: "DASH", video: "VIDEO", audio: "AUDIO", page: "YOUTUBE" };

/** A readable name for a media address. */
function nameOf(entry, pageTitle) {
  if (entry.kind === "page") return (pageTitle ?? "").replace(/^\(\d+\)\s*/, "").replace(/\s*-\s*YouTube( Music)?$/, "").trim() || "This video";
  let file = "";
  try {
    const path = new URL(entry.url).pathname;
    file = decodeURIComponent(path.split("/").filter(Boolean).pop() ?? "");
  } catch {
    /* fall through */
  }
  const generic = /^(master|index|playlist|chunklist|manifest|stream|video|main|prog_index)\.(m3u8|mpd)$/i;
  if ((entry.kind === "hls" || entry.kind === "dash") && (!file || generic.test(file)) && pageTitle) return pageTitle;
  return file || pageTitle || "Media";
}

function hostOf(url) {
  try {
    return new URL(url).hostname.replace(/^www\./, "");
  } catch {
    return "";
  }
}

function showStatus(host) {
  if (host.running) {
    statusLine.dataset.state = "running";
    statusText.textContent = host.version ? `Connected  ·  v${host.version}` : "Connected";
  } else if (host.installed) {
    statusLine.dataset.state = "ready";
    statusText.textContent = "Ready. Siphon starts when needed";
  } else {
    statusLine.dataset.state = "missing";
    statusText.textContent = "Siphon is not reachable";
  }
  openButton.disabled = !host.installed;
  notice.hidden = host.installed;
  if (!host.installed) {
    $("notice-title").textContent = "Open Siphon once";
    $("notice-body").textContent = "The app registers itself with this browser the first time it runs. Until then, downloads stay in the browser.";
  }
}

function renderMedia(media, tab) {
  list.replaceChildren();
  countLabel.textContent = media.length ? String(media.length) : "";
  emptyNote.hidden = media.length > 0;

  for (const entry of media) {
    const item = document.createElement("li");
    item.className = "item";

    const text = document.createElement("div");
    text.className = "item-text";
    const name = document.createElement("span");
    name.className = "item-name";
    name.textContent = nameOf(entry, tab?.title);
    name.title = entry.url;

    const meta = document.createElement("span");
    meta.className = "item-meta";
    const tag = document.createElement("span");
    tag.className = "tag";
    tag.dataset.kind = entry.kind;
    tag.textContent = KIND_LABEL[entry.kind] ?? "FILE";
    const host = document.createElement("span");
    host.className = "item-host";
    host.textContent = hostOf(entry.url);
    meta.append(tag, host);
    if (entry.quality) {
      const quality = document.createElement("span");
      quality.className = "num";
      quality.textContent = entry.quality;
      meta.append(quality);
    }
    const size = formatBytes(entry.size);
    if (size) {
      const number = document.createElement("span");
      number.className = "num";
      number.textContent = size;
      meta.append(number);
    }
    text.append(name, meta);

    const button = document.createElement("button");
    button.type = "button";
    button.className = "key primary";
    button.textContent = "Download";
    if (entry.live) {
      // A broadcast still in progress has no end to download to.
      tag.textContent = "LIVE";
      tag.dataset.kind = "dash";
      button.className = "key";
      button.disabled = true;
      button.textContent = "Live";
      button.title = "Live streams can be saved once they have ended.";
    } else if (entry.kind === "dash") {
      // Detected so the user knows it is there; assembling DASH is not built yet.
      button.className = "key";
      button.disabled = true;
      button.textContent = "Not yet";
      button.title = "DASH streams are detected but cannot be saved yet.";
    }
    button.addEventListener("click", async () => {
      button.dataset.busy = "true";
      try {
        await request({ type: "download", url: entry.url, kind: entry.kind, size: entry.size, mime: entry.mime, tabId: tab?.id });
        button.dataset.done = "true";
        button.textContent = "Sent";
        button.disabled = true;
      } catch (error) {
        button.textContent = "Retry";
        button.title = String(error?.message ?? error);
      } finally {
        delete button.dataset.busy;
      }
    });

    item.append(text, button);
    list.append(item);
  }
}

/** Set when the popup is opened as a page for one particular tab. */
const forcedTab = Number.parseInt(new URLSearchParams(location.search).get("tab") ?? "", 10);

async function load() {
  let state;
  try {
    state = await request({ type: "state", tabId: Number.isInteger(forcedTab) ? forcedTab : undefined });
  } catch (error) {
    statusLine.dataset.state = "missing";
    statusText.textContent = "The extension is restarting";
    list.replaceChildren();
    emptyNote.hidden = false;
    emptyNote.textContent = String(error?.message ?? error);
    return;
  }

  showStatus(state.host);
  enabledSwitch.checked = state.settings.enabled;
  sniffSwitch.checked = state.settings.sniffMedia;
  enabledSwitch.disabled = false;
  sniffSwitch.disabled = false;

  skipHosts = state.settings.skipHosts ?? [];
  site = /^https?:/i.test(state.tab?.url ?? "") ? hostOf(state.tab.url) : "";
  siteRow.hidden = !site;
  if (site) {
    $("site-title").textContent = `Capture on ${site}`;
    siteSwitch.checked = !skipHosts.some((skip) => site === skip || site.endsWith(`.${skip}`));
    siteSwitch.disabled = false;
  }
  videoSwitch.checked = state.settings.videoButton;
  videoSwitch.disabled = false;
  minSize.value = String(state.settings.minSizeKB);
  // A size set elsewhere that is not one of the choices reads as the nearest one below.
  if (minSize.selectedIndex < 0) minSize.value = "0";
  minSize.disabled = false;
  if (document.activeElement !== skipTypes) skipTypes.value = state.settings.skipTypes ?? "";
  skipTypes.disabled = false;
  grabButton.disabled = !site || !state.host.installed;
  renderMedia(state.settings.sniffMedia ? state.media : [], state.tab);
  if (!state.settings.sniffMedia) {
    emptyNote.hidden = false;
    emptyNote.textContent = "Media detection is off.";
  }
}

enabledSwitch.addEventListener("change", () => request({ type: "set", patch: { enabled: enabledSwitch.checked } }).catch(load));
sniffSwitch.addEventListener("change", () => request({ type: "set", patch: { sniffMedia: sniffSwitch.checked } }).then(load, load));

siteSwitch.addEventListener("change", () => {
  const rest = skipHosts.filter((skip) => !(site === skip || site.endsWith(`.${skip}`)));
  const next = siteSwitch.checked ? rest : [...rest, site];
  request({ type: "set", patch: { skipHosts: next } }).then(load, load);
});
videoSwitch.addEventListener("change", () => request({ type: "set", patch: { videoButton: videoSwitch.checked } }).catch(load));
minSize.addEventListener("change", () => request({ type: "set", patch: { minSizeKB: Number(minSize.value) || 0 } }).catch(load));
skipTypes.addEventListener("change", () => {
  const tidy = skipTypes.value.toLowerCase().split(/[\s,;.]+/).filter(Boolean).join(" ");
  skipTypes.value = tidy;
  request({ type: "set", patch: { skipTypes: tidy } }).catch(load);
});

grabButton.addEventListener("click", async () => {
  grabButton.dataset.busy = "true";
  grabNote.hidden = true;
  try {
    const reply = await request({ type: "grab", tabId: Number.isInteger(forcedTab) ? forcedTab : undefined });
    grabNote.dataset.state = "ok";
    grabNote.textContent = `${reply.count} ${reply.count === 1 ? "link" : "links"} sent to Siphon.`;
    grabNote.hidden = false;
  } catch (error) {
    grabNote.dataset.state = "bad";
    grabNote.textContent = String(error?.message ?? error);
    grabNote.hidden = false;
  } finally {
    delete grabButton.dataset.busy;
  }
});

openButton.addEventListener("click", async () => {
  openButton.dataset.busy = "true";
  try {
    await request({ type: "raise" });
    window.close();
  } catch (error) {
    statusLine.dataset.state = "missing";
    statusText.textContent = String(error?.message ?? error);
  } finally {
    delete openButton.dataset.busy;
  }
});

load();
