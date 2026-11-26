// Siphon browser extension: page script.
//
// Two jobs:
//   1. Tell the service worker when a link is clicked with Alt held, so that
//      one download stays in the browser.
//   2. Put a download button on the video the user is looking at: the one
//      under the pointer, else the one playing. On a feed with many videos
//      it belongs to that one post. Its menu lists the qualities the video
//      comes in; the one picked is what Siphon downloads.
//
// The button sits in a closed shadow root: the page cannot style it or read
// it, and it does not touch the page's own elements.

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

  const TOP = window.top === window;
  const HOST = location.hostname.replace(/^(www|m|mobile|web|music|old|new)\./, "");
  const on = (...names) => names.some((name) => HOST === name || HOST.endsWith(`.${name}`));

  // Inside another site's page, only a known player gets a button of its
  // own: a frame of unknown origin is as likely an advertisement.
  const EMBEDS = ["youtube.com", "youtube-nocookie.com", "player.vimeo.com", "dailymotion.com", "player.twitch.tv", "clips.twitch.tv", "streamable.com", "platform.twitter.com", "facebook.com", "tiktok.com"];
  if (!TOP && !on(...EMBEDS)) return;

  const MIN_WIDTH = 220;
  const MIN_HEIGHT = 124;

  // ------------------------------------------------------------- the DOM --

  /** `closest`, climbing out of shadow trees as well. */
  function closest(node, selector) {
    let at = node;
    while (at) {
      if (at.nodeType === 1) {
        const found = at.closest(selector);
        if (found) return found;
      }
      const root = at.getRootNode?.();
      at = root && root !== at && root.host ? root.host : null;
    }
    return null;
  }

  /** Hosts of open shadow trees, found by an occasional full walk. */
  let shadowHosts = [];
  let walkedAt = 0;

  function videos() {
    const found = [...document.querySelectorAll("video")];
    const now = Date.now();
    if (now - walkedAt > 2500) {
      walkedAt = now;
      shadowHosts = [];
      const walk = (root, depth) => {
        for (const element of root.querySelectorAll("*")) {
          if (element.shadowRoot) {
            shadowHosts.push(element);
            if (depth < 4) walk(element.shadowRoot, depth + 1);
          }
        }
      };
      // A page of tens of thousands of elements is walked rarely enough.
      walk(document, 0);
    }
    for (const host of shadowHosts) {
      if (host.isConnected && host.shadowRoot) found.push(...host.shadowRoot.querySelectorAll("video"));
    }
    return found;
  }

  function visibleRect(video) {
    const rect = video.getBoundingClientRect();
    if (rect.width < MIN_WIDTH || rect.height < MIN_HEIGHT) return null;
    const width = Math.min(rect.right, innerWidth) - Math.max(rect.left, 0);
    const height = Math.min(rect.bottom, innerHeight) - Math.max(rect.top, 0);
    if (width < MIN_WIDTH * 0.6 || height < MIN_HEIGHT * 0.6) return null;
    return { rect, shown: width * height };
  }

  const text = (node, limit = 140) => (node?.textContent ?? "").replace(/\s+/g, " ").trim().slice(0, limit);
  const absolute = (href) => {
    try {
      return new URL(href, location.href).href;
    } catch {
      return "";
    }
  };

  // ------------------------------------------------- which post, which page --
  //
  // Each site keeps the address of a post somewhere near its player. What
  // comes back is that address: Siphon reads the page behind it, so the
  // download is that one video in the quality asked for, under its own name.

  /** Whether an address is, by its shape, the page of one video. */
  function isVideoPage(address) {
    let url;
    try {
      url = new URL(address);
    } catch {
      return false;
    }
    if (!/^https?:$/.test(url.protocol)) return false;
    const host = url.hostname.toLowerCase().replace(/^(www|m|mobile|web|music|old|new)\./, "");
    const parts = url.pathname.split("/").filter(Boolean);
    const digits = (part) => /^\d+$/.test(part ?? "");
    const pair = (name, test) => parts.some((part, i) => part === name && test(parts[i + 1]));
    switch (host) {
      case "youtu.be":
        return parts.length > 0;
      case "youtube.com":
      case "youtube-nocookie.com":
        return (url.pathname === "/watch" && Boolean(url.searchParams.get("v"))) || (["shorts", "embed", "live", "v"].includes(parts[0]) && parts.length > 1);
      case "x.com":
      case "twitter.com":
        return pair("status", digits);
      case "facebook.com":
        return (["watch", "video.php"].includes(parts[0]) && Boolean(url.searchParams.get("v"))) || parts.includes("videos") || (parts[0] === "reel" && parts.length > 1) || (parts[0] === "share" && ["v", "r"].includes(parts[1]) && parts.length > 2);
      case "fb.watch":
      case "dai.ly":
      case "vm.tiktok.com":
      case "vt.tiktok.com":
        return parts.length > 0;
      case "instagram.com":
        return parts.length >= 2 && ["reel", "reels", "tv"].includes(parts[parts.length - 2]);
      case "tiktok.com":
        return pair("video", digits);
      case "vimeo.com":
        return digits(parts[parts.length - 1]);
      case "player.vimeo.com":
        return parts[0] === "video" && digits(parts[1]);
      case "dailymotion.com":
        return parts[0] === "video" && parts.length > 1;
      case "twitch.tv":
        return (parts[0] === "videos" && digits(parts[1])) || parts[1] === "clip";
      case "clips.twitch.tv":
      case "v.redd.it":
      case "streamable.com":
        return parts.length === 1;
      default:
        return false;
    }
  }

  const cleanTitle = () =>
    document.title
      .replace(/^\(\d+\)\s*/, "")
      .replace(/\s*[-|–—]\s*(YouTube( Music)?|Vimeo|Dailymotion|Twitch|Streamable)$/i, "")
      .trim();

  function youtube(video) {
    const params = new URLSearchParams(location.search);
    if (location.pathname === "/watch" && params.get("v")) return { url: location.href, title: cleanTitle() };
    if (location.pathname.startsWith("/shorts/")) return { url: location.href, title: cleanTitle() };
    const embedded = /^\/embed\/([\w-]{6,})/.exec(location.pathname);
    if (embedded) return { url: `https://www.youtube.com/watch?v=${embedded[1]}`, title: cleanTitle() };
    // A preview playing inside a list of videos: the tile it belongs to.
    const tile = closest(video, "ytd-rich-item-renderer, ytd-video-renderer, ytd-compact-video-renderer, ytd-grid-video-renderer, ytd-reel-video-renderer");
    const link = tile?.querySelector('a[href*="/watch?v="], a[href*="/shorts/"]');
    if (link) return { url: absolute(link.getAttribute("href")), title: text(tile.querySelector("#video-title, h3")) };
    return null;
  }

  function xcom(video) {
    const article = closest(video, "article");
    const stamp = article?.querySelector('a[href*="/status/"] time');
    let url = stamp ? absolute(closest(stamp, "a").getAttribute("href")) : /\/status\/\d+/.test(location.pathname) ? location.href : "";
    if (!url) return null;
    url = url.replace(/\/(photo|video|analytics|likes|retweets|quotes)(\/\d+)?\/?$/, "");
    if (article) {
      // A post can carry several videos; the address says which.
      const players = [...article.querySelectorAll('[data-testid="videoPlayer"], [data-testid="videoComponent"]')].filter((player, _, all) => !all.some((other) => other !== player && other.contains(player)));
      const index = players.findIndex((player) => player.contains(video));
      if (players.length > 1 && index >= 0) url += `/video/${index + 1}`;
    }
    const author = text(article?.querySelector('[data-testid="User-Name"] span'), 60);
    return { url, title: text(article?.querySelector('[data-testid="tweetText"]')), author };
  }

  const FACEBOOK_VIDEO = /\/(videos|reel)\/[\w.-]+|\/watch\/?\?(.*&)?v=\d+|\/share\/[vr]\/[\w-]+|\/video\.php\?(.*&)?v=\d+|fb\.watch\/[\w-]+/;

  function facebook(video) {
    if (!TOP) {
      // The embedded player names its video in its own address.
      const embedded = new URLSearchParams(location.search).get("href");
      return embedded ? { url: absolute(embedded), title: cleanTitle() } : null;
    }
    const post = closest(video, '[role="article"], [data-pagelet^="FeedUnit"], [data-pagelet="WatchPermalinkVideo"], [role="dialog"]');
    const describe = (url) => ({
      url,
      title: text(post?.querySelector('[data-ad-preview="message"], [data-ad-comet-preview="message"], [data-ad-rendering-role="story_message"]')),
      author: text(post?.querySelector("h2 a, h3 a, h4 a, strong a"), 60),
    });
    // The post links to its own video: around the player, or as its date.
    for (const link of post?.querySelectorAll("a[href]") ?? []) {
      const href = absolute(link.getAttribute("href"));
      if (FACEBOOK_VIDEO.test(href)) return describe(href);
    }
    // Opened on its own, the video's address is the page's.
    if (FACEBOOK_VIDEO.test(location.href)) return describe(location.href);
    for (const link of post?.querySelectorAll('a[href*="/posts/"], a[href*="/permalink/"], a[href*="story_fbid="]') ?? []) {
      return describe(absolute(link.getAttribute("href")));
    }
    return null;
  }

  function instagram(video) {
    const post = closest(video, 'article, [role="dialog"], main');
    const link = post?.querySelector('a[href*="/reel/"], a[href*="/p/"], a[href*="/tv/"]');
    const own = /^\/(?:[\w.]+\/)?(reels?|p|tv)\/([\w-]+)/.exec(location.pathname);
    const url = own ? `https://www.instagram.com/${own[1] === "reels" ? "reel" : own[1]}/${own[2]}/` : link ? absolute(link.getAttribute("href")) : "";
    if (!url) return null;
    return { url, title: text(post?.querySelector("h1, ul li span[dir]")), author: text(post?.querySelector("header a, a[role='link'] span"), 60) };
  }

  function reddit(video) {
    const post = closest(video, 'shreddit-post, [data-testid="post-container"], .thing[data-permalink], article');
    const link = post?.getAttribute?.("permalink") || post?.dataset?.permalink || post?.querySelector?.('a[data-click-id="comments"], a[slot="full-post-link"], a[href*="/comments/"]')?.getAttribute("href");
    const url = link ? absolute(link) : /\/comments\//.test(location.pathname) ? location.href : "";
    if (!url) return null;
    return {
      url,
      title: post?.getAttribute?.("post-title") || text(post?.querySelector?.('h1, h3, [slot="title"], a.title')),
      author: post?.getAttribute?.("subreddit-prefixed-name") || text(post?.querySelector?.('a[href^="/r/"]'), 60),
    };
  }

  function tiktok(video) {
    if (isVideoPage(location.href)) return { url: location.href, title: cleanTitle() };
    const embedded = /^\/embed\/(?:v\d\/)?(\d+)/.exec(location.pathname);
    // The player's own wrapper carries the video's number.
    const wrapper = closest(video, '[id^="xgwrapper-"]');
    const id = embedded?.[1] ?? /(\d{15,})$/.exec(wrapper?.id ?? "")?.[1];
    if (!id) return null;
    const item = closest(video, '[data-e2e="recommend-list-item-container"], article, [class*="DivItemContainer"]');
    const author = text(item?.querySelector('[data-e2e="video-author-uniqueid"], a[href^="/@"]'), 40).replace(/^@/, "");
    return { url: `https://www.tiktok.com/@${author || "_"}/video/${id}`, title: text(item?.querySelector('[data-e2e="video-desc"]')), author };
  }

  function embeddedPlayer() {
    const params = new URLSearchParams(location.search);
    if (on("player.vimeo.com")) return { url: location.href, title: cleanTitle() };
    if (on("dailymotion.com")) {
      const id = /\/(?:embed\/)?video\/(\w+)/.exec(location.pathname)?.[1];
      return id ? { url: `https://www.dailymotion.com/video/${id}`, title: cleanTitle() } : null;
    }
    if (on("player.twitch.tv")) {
      const id = /^v?(\d+)$/.exec(params.get("video") ?? "")?.[1];
      return id ? { url: `https://www.twitch.tv/videos/${id}`, title: cleanTitle() } : null;
    }
    if (on("clips.twitch.tv")) {
      const clip = params.get("clip") ?? location.pathname.split("/").filter(Boolean)[0];
      return clip && clip !== "embed" ? { url: `https://clips.twitch.tv/${clip}`, title: cleanTitle() } : null;
    }
    if (on("platform.twitter.com")) {
      const id = params.get("id");
      return id ? { url: `https://x.com/i/status/${id}`, title: "" } : null;
    }
    return null;
  }

  /**
   * Where a video lives: the address of its post or page, and what the page
   * says about it. `null` when this site keeps no such address for it.
   */
  function pageOf(video) {
    let found = null;
    if (on("youtube.com", "youtube-nocookie.com")) found = youtube(video);
    else if (on("x.com", "twitter.com") && !on("platform.twitter.com")) found = xcom(video);
    else if (on("facebook.com")) found = facebook(video);
    else if (on("instagram.com")) found = instagram(video);
    else if (on("reddit.com")) found = reddit(video);
    else if (on("tiktok.com")) found = tiktok(video);
    else if (!TOP) found = embeddedPlayer();
    // A site that gives each video a page of its own.
    else if (isVideoPage(location.href)) found = { url: location.href, title: cleanTitle() };
    else found = post(video);
    return found?.url && /^https?:/.test(found.url) ? found : null;
  }

  /**
   * Any other site that shows posts in a feed: by a convention most of them
   * follow, a post is an `article` whose date links to the post itself. Only
   * for a player that has no file address of its own to offer.
   */
  function post(video) {
    if (/^https?:/.test(video.currentSrc || video.src || "")) return null;
    const article = closest(video, 'article, [role="article"]');
    const stamp = article?.querySelector("a[href] time");
    const link = stamp ? closest(stamp, "a") : article?.querySelector('a[rel~="bookmark"]');
    const url = link ? absolute(link.getAttribute("href")) : "";
    let same = false;
    try {
      same = new URL(url).hostname === location.hostname && new URL(url).pathname !== location.pathname;
    } catch {
      /* not an address */
    }
    if (!same) return null;
    return { url, title: text(article.querySelector("h1, h2, h3, p")), author: text(article.querySelector('[rel~="author"], address, header a'), 60) };
  }

  /** Sites handled above: there the button always belongs to a post. */
  const KNOWN = on("youtube.com", "youtube-nocookie.com", "x.com", "twitter.com", "facebook.com", "instagram.com", "reddit.com", "tiktok.com", "vimeo.com", "dailymotion.com", "twitch.tv", "streamable.com");

  // ------------------------------------------------------------ the button --

  const STYLE = `
    :host { all: initial; }
    * { box-sizing: border-box; }
    .wrap { font: 510 12.5px/1 system-ui, -apple-system, "Segoe UI", sans-serif; color: #ecedee; -webkit-font-smoothing: antialiased;
            display: flex; flex-direction: column; align-items: flex-end; }
    .bar { display: flex; align-items: center; height: 30px; border-radius: 8px; background: #17181b; border: 1px solid rgb(255 255 255 / 0.16);
           box-shadow: inset 0 1px 0 0 rgb(255 255 255 / 0.08), 0 6px 20px rgb(0 0 0 / 0.5); overflow: hidden; }
    button { all: unset; box-sizing: border-box; display: flex; align-items: center; gap: 7px; cursor: pointer; color: inherit; font: inherit;
             transition: background-color 110ms ease-out, color 110ms ease-out; }
    .bar button { height: 100%; }
    button:hover { background: #1d1e22; }
    button:active { background: #131417; }
    button:focus-visible { outline: 2px solid #7faeff; outline-offset: -2px; }
    .main { padding: 0 9px 0 9px; }
    .main .arrow { color: #4c8dff; }
    .main .chevron { color: #9a9ca5; margin-left: -1px; transition: transform 140ms ease-out; }
    .main[aria-expanded="true"] .chevron { transform: rotate(180deg); }
    .main[data-state="done"] .arrow { color: #3dd68c; }
    .close { width: 26px; justify-content: center; color: #9a9ca5; border-left: 1px solid rgb(255 255 255 / 0.1); }
    .close:hover { color: #ecedee; }

    .menu { margin-top: 6px; width: 292px; border-radius: 10px; background: #141518; border: 1px solid rgb(255 255 255 / 0.16);
            box-shadow: inset 0 1px 0 0 rgb(255 255 255 / 0.07), 0 16px 40px rgb(0 0 0 / 0.6); overflow: hidden;
            animation: rise 120ms ease-out; }
    .menu[hidden] { display: none; }
    @keyframes rise { from { opacity: 0; transform: translateY(-4px); } to { opacity: 1; transform: none; } }
    .head { padding: 10px 12px 9px; border-bottom: 1px solid rgb(255 255 255 / 0.08); }
    .title { font-size: 13px; font-weight: 510; line-height: 17px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .sub { margin-top: 3px; font-size: 11.5px; font-weight: 400; line-height: 14px; color: #9a9ca5; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .label { padding: 9px 12px 4px; font-size: 10.5px; font-weight: 600; letter-spacing: 0.07em; color: #62656f; }
    .rows { padding: 0 4px 4px; max-height: 268px; overflow-y: auto; scrollbar-width: thin; scrollbar-color: #2a2c31 transparent; }
    .row { width: 100%; height: 34px; padding: 0 8px; border-radius: 6px; gap: 10px; }
    .row .q { flex: none; min-width: 58px; font-size: 13px; font-weight: 510; font-variant-numeric: tabular-nums; }
    .row .d { flex: 1; min-width: 0; font-size: 11.5px; font-weight: 400; color: #9a9ca5; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .row .s { flex: none; font: 400 11.5px/1 ui-monospace, "SF Mono", Menlo, monospace; color: #9a9ca5; font-variant-numeric: tabular-nums; }
    .row.usual { background: rgb(76 141 255 / 0.14); }
    .row.usual .q { color: #7faeff; }
    .row.usual:hover { background: rgb(76 141 255 / 0.22); }
    .row.usual:active { background: rgb(76 141 255 / 0.1); }
    .row.wide .q { min-width: 0; flex: 1; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .row.wide .d { flex: none; }
    .row[disabled] { cursor: default; }
    .row[disabled]:hover { background: none; }
    .row.usual[disabled]:hover { background: rgb(76 141 255 / 0.14); }
    .row .s[data-state="busy"] { color: #7faeff; }
    .row .s[data-state="done"] { color: #3dd68c; }
    .row .s[data-state="bad"] { color: #f2555a; }
    .skeleton { height: 34px; padding: 0 8px; display: flex; align-items: center; gap: 10px; }
    .bone { height: 9px; border-radius: 4px; background: rgb(255 255 255 / 0.09); animation: pulse 1.1s ease-in-out infinite; }
    @keyframes pulse { 50% { background: rgb(255 255 255 / 0.16); } }
    .note { padding: 12px 12px 4px; font-size: 12.5px; font-weight: 510; line-height: 17px; }
    .why { padding: 0 12px 10px; font-size: 11.5px; font-weight: 400; line-height: 16px; color: #9a9ca5; overflow-wrap: anywhere; }
    .again { margin: 0 8px 8px; height: 28px; padding: 0 11px; border-radius: 6px; background: #1d1e22; border: 1px solid rgb(255 255 255 / 0.12);
             box-shadow: inset 0 1px 0 0 rgb(255 255 255 / 0.06); font-size: 12px; }
    .again:hover { background: #24262b; }
    .foot { padding: 8px 12px 9px; border-top: 1px solid rgb(255 255 255 / 0.08); font-size: 11px; font-weight: 400; line-height: 14px; color: #62656f; }
    @media (prefers-reduced-motion: reduce) { button, .main .chevron { transition: none; } .menu, .bone { animation: none; } }
  `;

  const ARROW = `<svg class="arrow" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8 2.5v8M4.5 7.5 8 11l3.5-3.5M3 13.5h10"/></svg>`;
  const CHEVRON = `<svg class="chevron" width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2 3.75 5 6.75l3-3"/></svg>`;
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

  function clock(seconds) {
    if (!Number.isFinite(seconds) || seconds <= 0) return "";
    const whole = Math.round(seconds);
    const [h, m, s] = [Math.floor(whole / 3600), Math.floor(whole / 60) % 60, whole % 60];
    return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}` : `${m}:${String(s).padStart(2, "0")}`;
  }

  function element(tag, className, content) {
    const made = document.createElement(tag);
    if (className) made.className = className;
    if (content !== undefined) made.textContent = content;
    if (tag === "button") made.type = "button";
    return made;
  }

  /** What the tab's own requests gave away, for sites without post pages. */
  let sniffed = [];
  let buttonAllowed = true;
  /** The video the button is on; `null` puts it at the window's corner. */
  let target = null;
  let pointer = null;
  /** Videos whose button was closed, until the page moves on. */
  let hidden = new WeakSet();
  let cornerHidden = false;

  /** @type {HTMLElement | null} */
  let host = null;
  /** @type {ShadowRoot | null} */
  let root = null;
  let main = null;
  let menu = null;
  let open = false;
  /** What the open menu shows: `{ video, state, ... }`. */
  let shown = null;
  /** Answers already fetched, by page address. */
  const known = new Map();

  const usable = () => sniffed.filter((entry) => !entry.live && entry.kind !== "dash");

  /** The video the user means: under the pointer, else playing, else largest. */
  function choose() {
    const candidates = [];
    for (const video of videos()) {
      if (hidden.has(video)) continue;
      const seen = visibleRect(video);
      if (seen) candidates.push({ video, ...seen });
    }
    if (candidates.length === 0) return null;
    if (pointer) {
      const under = candidates.filter(({ rect }) => pointer.x >= rect.left && pointer.x <= rect.right && pointer.y >= rect.top && pointer.y <= rect.bottom);
      // The innermost one, where players are nested.
      if (under.length > 0) return under.reduce((a, b) => (a.rect.width * a.rect.height <= b.rect.width * b.rect.height ? a : b)).video;
    }
    const playing = candidates.filter(({ video }) => !video.paused && !video.ended && video.readyState > 2);
    const pool = playing.length > 0 ? playing : candidates;
    return pool.reduce((a, b) => (a.shown >= b.shown ? a : b)).video;
  }

  /** Whether there is anything to offer for a video. */
  function offers(video) {
    if (pageOf(video)) return true;
    if (KNOWN) return false; // a preview or an advertisement: no post behind it
    if (/^https?:/.test(video.currentSrc || video.src || "")) return true;
    return usable().length > 0;
  }

  function place() {
    if (!host) return;
    const rect = target?.isConnected ? target.getBoundingClientRect() : null;
    // By the player's top right corner; without a player, the window's.
    const top = rect ? Math.min(Math.max(8, rect.top + 10), innerHeight - 40) : 12;
    // Measured from the page's own edge: a scroll bar is not part of it.
    const width = document.documentElement.clientWidth || innerWidth;
    const right = rect ? Math.min(Math.max(8, width - rect.right + 10), width - 120) : 16;
    host.style.top = `${Math.round(top)}px`;
    host.style.right = `${Math.round(right)}px`;
  }

  function remove() {
    host?.remove();
    host = null;
    root = null;
    main = null;
    menu = null;
    open = false;
    shown = null;
  }

  function build() {
    host = document.createElement("siphon-button");
    host.style.cssText = "all: initial; position: fixed; z-index: 2147483646; top: 12px; right: 16px;";
    root = host.attachShadow({ mode: "closed" });
    const style = element("style");
    style.textContent = STYLE;
    const wrap = element("div", "wrap");
    const bar = element("div", "bar");
    main = element("button", "main");
    main.title = "Download with Siphon";
    main.setAttribute("aria-haspopup", "true");
    main.setAttribute("aria-expanded", "false");
    main.innerHTML = `${ARROW}<span class="text">Download</span>${CHEVRON}`;
    const close = element("button", "close");
    close.title = "Hide for this video";
    close.setAttribute("aria-label", "Hide the download button for this video");
    close.innerHTML = CROSS;
    bar.append(main, close);
    menu = element("div", "menu");
    menu.hidden = true;
    menu.setAttribute("role", "menu");
    wrap.append(bar, menu);
    root.append(style, wrap);
    (document.body ?? document.documentElement).append(host);

    main.addEventListener("click", toggle);
    // The qualities are on their way by the time the menu is asked for.
    main.addEventListener("pointerenter", () => {
      const page = target && pageOf(target);
      if (page) describe(page);
    });
    close.addEventListener("click", () => {
      if (target) hidden.add(target);
      else cornerHidden = true;
      target = null;
      remove();
      refresh();
    });
    // The page must not take these for its own (a click that pauses the video).
    for (const type of ["click", "dblclick", "mousedown", "mouseup", "pointerdown", "pointerup", "contextmenu"]) {
      host.addEventListener(type, (event) => event.stopPropagation());
    }
  }

  function setLabel(label, state) {
    if (!main) return;
    main.querySelector(".text").textContent = label;
    main.dataset.state = state ?? "";
  }

  /** Decides where the button goes, or that there is none. */
  function refresh() {
    if (!buttonAllowed) {
      remove();
      return;
    }
    // An open menu belongs to its video until it is closed.
    if (open && target?.isConnected) {
      place();
      return;
    }
    const video = choose();
    const next = video && offers(video) ? video : null;
    const corner = !next && TOP && !KNOWN && !cornerHidden && usable().length > 0;
    if (!next && !corner) {
      target = null;
      remove();
      return;
    }
    if (!host) build();
    if (next !== target) {
      target = next;
      closeMenu();
      setLabel("Download");
    }
    place();
  }

  function closeMenu() {
    open = false;
    shown = null;
    if (menu) menu.hidden = true;
    main?.setAttribute("aria-expanded", "false");
  }

  /** Asks Siphon what a page offers; one request per address at a time. */
  function describe(page) {
    const kept = known.get(page.url);
    if (kept && (kept.pending || Date.now() - kept.at < 180_000)) return kept.promise;
    const entry = { pending: true, at: Date.now(), promise: null };
    entry.promise = send({ type: "describe", url: page.url, referer: TOP ? undefined : document.referrer || undefined }).then((reply) => {
      entry.pending = false;
      entry.at = Date.now();
      // A refusal is asked again next time: the user may have signed in since.
      if (!reply?.ok) known.delete(page.url);
      return reply ?? { ok: false, error: "Siphon did not answer." };
    });
    known.set(page.url, entry);
    return entry.promise;
  }

  /** The streams and files to offer when there is no page to read. */
  function loose(video) {
    const own = video ? video.currentSrc || video.src || "" : "";
    if (/^https?:/.test(own)) return [{ kind: "video", url: own, size: 0, mime: "", own: true }];
    return usable();
  }

  async function toggle() {
    if (open) {
      closeMenu();
      return;
    }
    const video = target;
    const page = video ? pageOf(video) : null;
    const items = page ? [] : loose(video);
    // One plain file: there is nothing to choose.
    if (!page && items.length === 1 && !(items[0].qualities?.length > 1)) {
      await deliver({ url: items[0].url, kind: items[0].kind, size: items[0].size, mime: items[0].mime, name: nameFor(items[0], video) });
      return;
    }
    open = true;
    main.setAttribute("aria-expanded", "true");
    menu.hidden = false;
    shown = { video };
    if (!page) {
      renderLoose(items, video, null);
      return;
    }
    renderLoading(page);
    const reply = await describe(page);
    if (!open || shown?.video !== video) return;
    if (reply.ok && reply.page?.variants?.length > 0) renderQualities(page, reply.page);
    else renderLoose(loose(video), video, reply.error || "Siphon could not read this page.", page);
  }

  function header(title, sub) {
    const head = element("div", "head");
    head.append(element("div", "title", title || "Video"));
    if (sub) head.append(element("div", "sub", sub));
    return head;
  }

  function renderLoading(page) {
    const rows = element("div", "rows");
    for (const widths of [[42, 96, 48], [42, 110, 44], [42, 88, 48], [42, 102, 40]]) {
      const line = element("div", "skeleton");
      widths.forEach((width, index) => {
        const bone = element("div", "bone");
        bone.style.width = `${width}px`;
        if (index === 1) bone.style.marginRight = "auto";
        line.append(bone);
      });
      rows.append(line);
    }
    menu.replaceChildren(header(page.title || "Video", page.author ? `${page.author}  ·  reading the page…` : "Reading the page…"), element("div", "label", "QUALITY"), rows);
  }

  function renderQualities(page, info) {
    const rows = element("div", "rows");
    for (const variant of info.variants) {
      const row = element("button", variant.id === info.default ? "row usual" : "row");
      row.setAttribute("role", "menuitem");
      const size = variant.size > 0 ? `${variant.size_estimated ? "≈ " : ""}${formatBytes(variant.size)}` : "";
      const state = element("span", "s", size);
      row.append(element("span", "q", variant.label), element("span", "d", variant.detail), state);
      row.title = variant.id === info.default ? "The quality Siphon takes by default" : "";
      row.addEventListener("click", () => deliver({ url: page.url, page: true, variant: variant.id, referer: TOP ? undefined : document.referrer || undefined }, row, state));
      rows.append(row);
    }
    const sub = [info.uploader, clock(info.duration)].filter(Boolean).join("  ·  ");
    menu.replaceChildren(header(info.name || page.title, sub), element("div", "label", "QUALITY"), rows, element("div", "foot", "Siphon starts downloading the one you pick."));
    rows.querySelector(".usual")?.scrollIntoView({ block: "nearest" });
  }

  function nameFor(entry, video) {
    // A name people would recognise: what the post or the page is called.
    const context = video ? text(closest(video, 'article, [role="article"], figure, section')?.querySelector("h1, h2, h3, figcaption"), 100) : "";
    let file = "";
    try {
      file = decodeURIComponent(new URL(entry.url).pathname.split("/").filter(Boolean).pop() ?? "");
    } catch {
      /* fall through */
    }
    const stem = file.replace(/\.[a-z0-9]{2,5}$/i, "");
    // A name made of a hash or a counter says nothing; the page says more.
    const opaque = !stem || /^(master|index|playlist|chunklist|manifest|stream|video|main|prog_index)$/i.test(stem) || (stem.length >= 16 && !/[\s._-]/.test(stem)) || (/[A-Za-z]/.test(stem) && /\d/.test(stem) && !/[aeiou]{2}|[a-z]{4}/i.test(stem));
    return opaque ? context || cleanTitle() || "Video" : stem;
  }

  function renderLoose(items, video, problem, page) {
    const parts = [];
    if (problem) {
      parts.push(header(page?.title || cleanTitle() || "Video", page?.author || location.hostname));
      parts.push(element("div", "note", "Siphon could not read this video’s page"), element("div", "why", problem.replace(/\.$/, "") + "."));
    } else {
      parts.push(header(cleanTitle() || "Video", location.hostname));
    }
    const rows = element("div", "rows");
    const single = items.length === 1 && items[0].qualities?.length > 1 ? items[0] : null;
    if (single) {
      // One stream in several qualities.
      single.qualities.forEach((quality, index) => {
        const row = element("button", index === 0 ? "row usual" : "row");
        row.setAttribute("role", "menuitem");
        const state = element("span", "s", "");
        row.append(element("span", "q", `${quality}p`), element("span", "d", "Stream"), state);
        row.addEventListener("click", () => deliver({ url: single.url, kind: "hls", variant: String(quality), name: nameFor(single, video) }, row, state));
        rows.append(row);
      });
      parts.push(element("div", "label", "QUALITY"));
    } else if (items.length > 0) {
      for (const entry of items.slice(0, 8)) {
        const row = element("button", "row wide");
        row.setAttribute("role", "menuitem");
        const kind = entry.kind === "hls" ? "Stream" : entry.kind === "audio" ? "Audio" : "Video";
        const detail = [kind, entry.quality].filter(Boolean).join("  ·  ");
        const state = element("span", "s", formatBytes(entry.size));
        row.append(element("span", "q", nameFor(entry, video)), element("span", "d", detail), state);
        row.addEventListener("click", () => deliver({ url: entry.url, kind: entry.kind, size: entry.size, mime: entry.mime, name: nameFor(entry, video) }, row, state));
        rows.append(row);
      }
      parts.push(element("div", "label", problem ? "FOUND ON THIS PAGE INSTEAD" : "FOUND ON THIS PAGE"));
    }
    if (rows.childElementCount > 0) parts.push(rows);
    if (problem && page) {
      const again = element("button", "again", "Try again");
      again.addEventListener("click", () => {
        closeMenu();
        toggle();
      });
      parts.push(again);
    }
    menu.replaceChildren(...parts);
  }

  /** Hands one choice to Siphon and says how that went. */
  async function deliver(what, row, state) {
    if (row?.hasAttribute("disabled")) return;
    for (const other of menu?.querySelectorAll(".row") ?? []) other.setAttribute("disabled", "");
    const before = state?.textContent ?? "";
    if (state) {
      state.dataset.state = "busy";
      state.textContent = "Sending…";
    }
    const reply = await send({ type: "download", ...what });
    if (reply?.ok) {
      if (state) {
        state.dataset.state = "done";
        state.textContent = "Sent";
      }
      setLabel("Sent to Siphon", "done");
      setTimeout(closeMenu, 700);
    } else {
      if (state) {
        state.dataset.state = "bad";
        state.textContent = "Not sent";
        state.title = reply?.error ?? "Siphon did not answer.";
        setTimeout(() => {
          if (state.isConnected) {
            state.dataset.state = "";
            state.textContent = before;
          }
        }, 2600);
      }
      setLabel("Siphon did not answer", "bad");
      for (const other of menu?.querySelectorAll(".row") ?? []) other.removeAttribute("disabled");
    }
    setTimeout(() => setLabel("Download"), 2600);
  }

  // ------------------------------------------------------------- upkeep --

  async function update() {
    const reply = await send({ type: "page-media" });
    if (reply?.ok) {
      sniffed = reply.media ?? [];
      buttonAllowed = Boolean(reply.videoButton);
    }
    refresh();
  }

  chrome.runtime.onMessage.addListener((message) => {
    if (message?.type === "siphon-media") update();
  });

  let queued = false;
  const soon = () => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => {
      queued = false;
      refresh();
    });
  };
  addEventListener(
    "pointermove",
    (event) => {
      // Over the button itself the pointer is still "on" its video.
      if (host && event.target === host) return;
      pointer = { x: event.clientX, y: event.clientY };
      soon();
    },
    { passive: true, capture: true },
  );
  addEventListener("scroll", soon, { passive: true, capture: true });
  addEventListener("resize", soon, { passive: true });
  // Videos start, stop and are swapped without the pointer moving.
  setInterval(refresh, 700);
  addEventListener(
    "pointerdown",
    (event) => {
      if (open && event.target !== host) closeMenu();
    },
    true,
  );
  addEventListener(
    "keydown",
    (event) => {
      if (open && event.key === "Escape") {
        closeMenu();
        event.stopPropagation();
      }
    },
    true,
  );

  // Sites that move from page to page without loading one: what was closed
  // or looked up belongs to the page that was left. A page that only touches
  // its own address (a playback position, a tracking tag) has not moved.
  const whereabouts = () => {
    const params = new URLSearchParams(location.search);
    return `${location.pathname}?${["v", "id", "story_fbid", "fbid"].map((name) => params.get(name) ?? "").join("&")}`;
  };
  let address = whereabouts();
  setInterval(() => {
    const now = whereabouts();
    if (now === address) return;
    address = now;
    hidden = new WeakSet();
    cornerHidden = false;
    closeMenu();
    update();
  }, 800);

  update();
})();
