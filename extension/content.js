// Siphon browser extension: page script.
//
// Two jobs, both small:
//   1. Tell the service worker when a link is clicked with Alt held, so that
//      one download stays in the browser.
//   2. On pages where a video or stream was noticed, and on YouTube watch
//      pages, show a download button by the player. It sits in a closed shadow root: the page cannot style
//      it or read it, and it does not touch the page's own elements.

(() => {
  const send = (message) => chrome.runtime.sendMessage(message).catch(() => null);

  // Chrome itself downloads an Alt-clicked link. Holding Alt is the user
  // saying "the browser, not Siphon, this once".
  addEventListener(
    "mousedown",
    (event) => {
      if (event.altKey && event.button === 0) send({ type: "bypass-next" });
    },
    true,
  );

  // The button belongs to the page as a whole, not to each frame in it.
  if (window.top !== window) return;

  const MIN_WIDTH = 220;
  const MIN_HEIGHT = 124;
  /** @type {HTMLElement | null} */
  let host = null;
  /** @type {ShadowRoot | null} */
  let root = null;
  let media = [];
  let dismissed = false;
  let open = false;
  let timer = 0;

  const STYLE = `
    :host { all: initial; }
    * { box-sizing: border-box; }
    .wrap { font: 510 12.5px/1 system-ui, -apple-system, "Segoe UI", sans-serif; color: #ecedee; -webkit-font-smoothing: antialiased; }
    .bar { display: flex; align-items: center; height: 30px; border-radius: 8px; background: #17181b; border: 1px solid rgb(255 255 255 / 0.14);
           box-shadow: inset 0 1px 0 0 rgb(255 255 255 / 0.08), 0 6px 20px rgb(0 0 0 / 0.45); overflow: hidden; }
    button { all: unset; display: flex; align-items: center; gap: 7px; height: 100%; cursor: pointer; color: inherit; font: inherit;
             transition: background-color 110ms ease-out; }
    button:hover { background: #1d1e22; }
    button:active { background: #131417; }
    button:focus-visible { outline: 2px solid #7faeff; outline-offset: -2px; }
    .main { padding: 0 11px 0 9px; }
    .main svg { color: #4c8dff; }
    .count { font: 510 11px/1 ui-monospace, "SF Mono", Menlo, monospace; padding: 2px 5px; border-radius: 4px; background: rgb(76 141 255 / 0.22); color: #7faeff; }
    .close { width: 26px; justify-content: center; color: #9a9ca5; border-left: 1px solid rgb(255 255 255 / 0.1); }
    .close:hover { color: #ecedee; }
    .menu { margin-top: 6px; min-width: 250px; max-width: 340px; padding: 5px; border-radius: 9px; background: #17181b; border: 1px solid rgb(255 255 255 / 0.14);
            box-shadow: 0 12px 32px rgb(0 0 0 / 0.55); }
    .menu[hidden] { display: none; }
    .item { width: 100%; height: auto; padding: 7px 8px; border-radius: 6px; justify-content: space-between; gap: 12px; }
    .item[disabled] { cursor: default; opacity: 0.6; }
    .item[disabled]:hover { background: none; }
    .text { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
    .name { font-weight: 510; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; max-width: 230px; }
    .meta { font: 400 11px/1 ui-monospace, "SF Mono", Menlo, monospace; color: #9a9ca5; }
    .state { flex: none; font-size: 11.5px; color: #7faeff; }
    .state[data-state="done"] { color: #3dd68c; }
    .state[data-state="bad"] { color: #f2555a; }
    @media (prefers-reduced-motion: reduce) { button { transition: none; } }
  `;

  const ARROW = `<svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8 2.5v8M4.5 7.5 8 11l3.5-3.5M3 13.5h10"/></svg>`;
  const CROSS = `<svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" aria-hidden="true"><path d="M2 2l6 6M8 2 2 8"/></svg>`;

  function formatBytes(bytes) {
    if (!Number.isFinite(bytes) || bytes <= 0) return "";
    const units = ["B", "KB", "MB", "GB"];
    let value = bytes;
    let unit = 0;
    while (value >= 1024 && unit < units.length - 1) {
      value /= 1024;
      unit += 1;
    }
    return `${value.toFixed(value >= 100 || unit === 0 ? 0 : 1)} ${units[unit]}`;
  }

  /**
   * A page whose video the application finds by reading the page address,
   * rather than this extension by watching requests: a YouTube watch page.
   */
  function videoPage() {
    const host = location.hostname.replace(/^(www|m|music)\./, "");
    if (host !== "youtube.com") return null;
    const watching = location.pathname === "/watch" && new URLSearchParams(location.search).get("v");
    if (!watching && !location.pathname.startsWith("/shorts/")) return null;
    // "(3) Title - YouTube" is the tab's title; the video's is the middle.
    const name = document.title.replace(/^\(\d+\)\s*/, "").replace(/\s*-\s*YouTube( Music)?$/, "").trim();
    return { kind: "page", url: location.href, name: name || "Video", size: 0, mime: "" };
  }

  function nameOf(entry) {
    if (entry.kind === "page") return videoPage()?.name ?? entry.name;
    let file = "";
    try {
      file = decodeURIComponent(new URL(entry.url).pathname.split("/").filter(Boolean).pop() ?? "");
    } catch {
      /* fall through */
    }
    const generic = /^(master|index|playlist|chunklist|manifest|stream|video|main|prog_index)\.(m3u8|mpd)$/i;
    if (entry.kind === "hls" && (!file || generic.test(file))) return document.title || "Stream";
    return file || document.title || "Media";
  }

  function describe(entry) {
    if (entry.kind === "page") return "Video  ·  quality is chosen in Siphon";
    const parts = [entry.kind === "hls" ? "Stream" : entry.kind === "audio" ? "Audio" : "Video"];
    if (entry.quality) parts.push(entry.quality);
    const size = formatBytes(entry.size);
    if (size) parts.push(size);
    return parts.join("  ·  ");
  }

  /** The largest video the user can currently see, if there is one. */
  function player() {
    let best = null;
    for (const video of document.querySelectorAll("video")) {
      const rect = video.getBoundingClientRect();
      const visible = rect.width >= MIN_WIDTH && rect.height >= MIN_HEIGHT && rect.bottom > 40 && rect.top < innerHeight - 40 && rect.right > 40 && rect.left < innerWidth - 40;
      if (visible && (!best || rect.width * rect.height > best.width * best.height)) best = rect;
    }
    return best;
  }

  function place() {
    if (!host) return;
    const rect = player();
    // By the player's top right corner; without a player, the window's.
    const top = rect ? Math.max(8, rect.top + 10) : 12;
    const right = rect ? Math.max(8, innerWidth - rect.right + 10) : 16;
    host.style.top = `${Math.round(top)}px`;
    host.style.right = `${Math.round(right)}px`;
  }

  function remove() {
    clearInterval(timer);
    host?.remove();
    host = null;
    root = null;
    open = false;
  }

  function render() {
    const usable = media.filter((entry) => !entry.live && entry.kind !== "dash");
    if (dismissed || usable.length === 0) {
      remove();
      return;
    }
    if (!host) {
      host = document.createElement("siphon-button");
      host.style.cssText = "all: initial; position: fixed; z-index: 2147483646; top: 12px; right: 16px;";
      root = host.attachShadow({ mode: "closed" });
      (document.body ?? document.documentElement).append(host);
      // Players move: pages scroll, resize and swap layouts.
      timer = setInterval(place, 600);
      addEventListener("scroll", place, { passive: true, capture: true });
      addEventListener("resize", place, { passive: true });
    }

    const wrap = document.createElement("div");
    wrap.className = "wrap";
    const style = document.createElement("style");
    style.textContent = STYLE;

    const bar = document.createElement("div");
    bar.className = "bar";
    const main = document.createElement("button");
    main.className = "main";
    main.type = "button";
    main.title = "Download with Siphon";
    main.innerHTML = ARROW;
    main.append(document.createTextNode(usable.length === 1 ? "Download this video" : "Download"));
    if (usable.length > 1) {
      const count = document.createElement("span");
      count.className = "count";
      count.textContent = String(usable.length);
      main.append(count);
    }
    const close = document.createElement("button");
    close.className = "close";
    close.type = "button";
    close.title = "Hide on this page";
    close.setAttribute("aria-label", "Hide the download button on this page");
    close.innerHTML = CROSS;
    bar.append(main, close);

    const menu = document.createElement("div");
    menu.className = "menu";
    menu.hidden = !open;
    for (const entry of usable.slice(0, 8)) {
      const item = document.createElement("button");
      item.className = "item";
      item.type = "button";
      const text = document.createElement("span");
      text.className = "text";
      const name = document.createElement("span");
      name.className = "name";
      name.textContent = nameOf(entry);
      const meta = document.createElement("span");
      meta.className = "meta";
      meta.textContent = describe(entry);
      text.append(name, meta);
      const state = document.createElement("span");
      state.className = "state";
      item.append(text, state);
      item.addEventListener("click", () => deliver(entry, item, state));
      menu.append(item);
    }

    main.addEventListener("click", () => {
      if (usable.length === 1 && !open) {
        const [item] = menu.children;
        deliver(usable[0], item, item.querySelector(".state"), main);
        return;
      }
      open = !open;
      menu.hidden = !open;
    });
    close.addEventListener("click", () => {
      dismissed = true;
      remove();
    });

    wrap.append(bar, menu);
    root.replaceChildren(style, wrap);
    place();
  }

  async function deliver(entry, item, state, label) {
    if (item.hasAttribute("disabled")) return;
    item.setAttribute("disabled", "");
    state.dataset.state = "";
    state.textContent = "Sending…";
    const original = label?.lastChild?.textContent;
    const target = entry.kind === "page" ? location.href : entry.url;
    const reply = await send({ type: "download", url: target, kind: entry.kind, size: entry.size, mime: entry.mime });
    if (reply?.ok) {
      state.dataset.state = "done";
      state.textContent = "Sent";
      if (label) label.lastChild.textContent = "Sent to Siphon";
    } else {
      state.dataset.state = "bad";
      state.textContent = "Not sent";
      state.title = reply?.error ?? "Siphon did not answer.";
      if (label) label.lastChild.textContent = "Siphon did not answer";
      item.removeAttribute("disabled");
    }
    if (label) {
      setTimeout(() => {
        if (label.isConnected) label.lastChild.textContent = original;
      }, 2600);
    }
  }

  async function refresh() {
    const reply = await send({ type: "page-media" });
    if (!reply?.ok) return;
    // On a video page the page itself is the one thing to offer: the pieces
    // its player requests are not files anyone could use.
    const page = videoPage();
    media = !reply.videoButton ? [] : page ? [page] : reply.media;
    render();
  }

  chrome.runtime.onMessage.addListener((message) => {
    if (message?.type === "siphon-media") refresh();
  });
  refresh();

  // Such sites move from video to video without loading a new page.
  if (/(^|\.)youtube\.com$/.test(location.hostname)) {
    let shown = location.href;
    const moved = () => {
      if (location.href === shown) return;
      shown = location.href;
      dismissed = false;
      open = false;
      refresh();
    };
    document.addEventListener("yt-navigate-finish", () => setTimeout(moved, 300));
    setInterval(moved, 1500);
  }
})();
