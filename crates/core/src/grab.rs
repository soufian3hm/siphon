//! The link grabber: every downloadable thing a page points to.
//!
//! Given a page (and optionally the pages it links to on the same site), it
//! collects the addresses of files, images and media so the user can pick
//! what to download. The browser extension produces the same [`LinkBatch`]
//! from a page that is already open, where cookies and scripts have done
//! their work.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

use reqwest::header;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::types::Request;

/// A page larger than this is not a page worth parsing.
const MAX_PAGE_BYTES: usize = 6 * 1024 * 1024;
/// Pages fetched at once while exploring a site.
const CONCURRENCY: usize = 4;
pub const MAX_PAGES: usize = 300;
pub const MAX_DEPTH: u8 = 3;
const MAX_LINKS: usize = 20_000;

/// How a link was found, which says what it probably is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkKind {
    /// A link whose address looks like a file.
    #[default]
    File,
    /// A link to another page (or to something with no telling name).
    Page,
    /// An image, video or sound the page itself displays.
    Embedded,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FoundLink {
    pub url: String,
    /// The link text, or the image's description.
    pub text: String,
    pub kind: LinkKind,
}

/// The links of one page or one exploration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkBatch {
    pub page_url: String,
    pub page_title: String,
    pub links: Vec<FoundLink>,
    /// Headers the browser sends from this page (cookies, user agent), so
    /// downloads started from the list are let in like the page was.
    pub headers: Vec<(String, String)>,
}

impl LinkBatch {
    /// The request for one of the batch's links.
    pub fn request_for(&self, url: &str) -> Request {
        let mut request = Request::new(url);
        if !self.page_url.trim().is_empty() {
            request.referer = Some(self.page_url.clone());
        }
        // Cookies belong to the page's site and must not follow a link to
        // somewhere else.
        let same_site = same_site(url, &self.page_url);
        for (name, value) in &self.headers {
            if same_site || !name.eq_ignore_ascii_case("cookie") {
                request.set_header(name, value.clone());
            }
        }
        request
    }
}

/// Extensions that are pages, not files.
const PAGE_EXTENSIONS: [&str; 12] = ["html", "htm", "xhtml", "shtml", "php", "php5", "asp", "aspx", "jsp", "jspx", "cfm", "cgi"];

fn extension_of(url: &url::Url) -> Option<String> {
    let name = url.path_segments()?.next_back()?;
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty() && ext.len() <= 10 && ext.chars().all(|c| c.is_ascii_alphanumeric())).then(|| ext.to_ascii_lowercase())
}

fn kind_of(url: &url::Url) -> LinkKind {
    match extension_of(url) {
        Some(ext) if !PAGE_EXTENSIONS.contains(&ext.as_str()) => LinkKind::File,
        _ => LinkKind::Page,
    }
}

fn site_of(url: &url::Url) -> Option<String> {
    Some(url.host_str()?.trim_start_matches("www.").to_ascii_lowercase())
}

fn same_site(a: &str, b: &str) -> bool {
    match (url::Url::parse(a).ok().as_ref().and_then(site_of), url::Url::parse(b).ok().as_ref().and_then(site_of)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn tidy(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.char_indices().nth(160) {
        Some((at, _)) => format!("{}\u{2026}", &joined[..at]),
        None => joined,
    }
}

/// What one HTML document points to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PageLinks {
    pub title: String,
    pub links: Vec<FoundLink>,
}

/// Extracts the links of an HTML document, resolved against its address.
pub fn links_in(html: &str, page: &url::Url) -> PageLinks {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);
    let select = |css: &str| Selector::parse(css).expect("static selector");

    // `<base href>` changes what relative addresses are relative to.
    let base = document
        .select(&select("base[href]"))
        .next()
        .and_then(|b| b.attr("href"))
        .and_then(|href| page.join(href.trim()).ok())
        .unwrap_or_else(|| page.clone());
    let title = document.select(&select("title")).next().map(|t| tidy(&t.text().collect::<String>())).unwrap_or_default();

    let mut links: Vec<FoundLink> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // `is_file` is the page itself saying "this is a download".
    let mut push = |raw: &str, text: String, embedded: bool, is_file: bool| {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            return;
        }
        let Ok(mut url) = base.join(raw) else { return };
        if !matches!(url.scheme(), "http" | "https" | "ftp") {
            return;
        }
        url.set_fragment(None);
        let kind = match (embedded, is_file) {
            (true, _) => LinkKind::Embedded,
            (false, true) => LinkKind::File,
            (false, false) => kind_of(&url),
        };
        let url = url.to_string();
        if links.len() < MAX_LINKS && seen.insert(url.clone()) {
            links.push(FoundLink { url, text, kind });
        }
    };

    for element in document.select(&select("a[href], area[href]")) {
        let text = tidy(&element.text().collect::<String>());
        // A link around a picture is named by the picture.
        let text = if text.is_empty() {
            element.attr("title").or_else(|| element.select(&select("img[alt]")).next().and_then(|img| img.attr("alt"))).map(tidy).unwrap_or_default()
        } else {
            text
        };
        push(element.attr("href").unwrap_or_default(), text, false, element.attr("download").is_some());
    }
    for element in document.select(&select("img, video, audio, source, embed, track")) {
        let text = element.attr("alt").or_else(|| element.attr("title")).map(tidy).unwrap_or_default();
        // Lazy-loading scripts keep the real address in a data attribute.
        for attribute in ["src", "data-src", "data-original", "data-lazy-src"] {
            if let Some(value) = element.attr(attribute)
                && !value.trim_start().starts_with("data:")
            {
                push(value, text.clone(), true, false);
            }
        }
        // `srcset` lists the same image at several sizes; the last is the largest.
        if let Some(best) = element.attr("srcset").and_then(|set| set.split(',').next_back()).and_then(|c| c.split_whitespace().next()) {
            push(best, text.clone(), true, false);
        }
    }
    PageLinks { title, links }
}

/// How far to look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrabOptions {
    /// 0 reads only the given page; 1 also reads the pages it links to on
    /// the same site; and so on.
    pub depth: u8,
    pub max_pages: usize,
}

impl Default for GrabOptions {
    fn default() -> Self {
        Self { depth: 0, max_pages: MAX_PAGES }
    }
}

/// Live numbers of an exploration.
#[derive(Debug, Default)]
pub struct GrabProgress {
    pub pages_read: AtomicUsize,
    pub pages_known: AtomicUsize,
    pub links_found: AtomicUsize,
}

async fn fetch_page(client: &reqwest::Client, request: &Request, url: &url::Url) -> Result<Option<String>> {
    let mut builder = client.get(url.clone()).header(header::ACCEPT, "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5");
    if let Some(referer) = request.referer.as_deref() {
        builder = builder.header(header::REFERER, referer);
    }
    let same_site = same_site(url.as_str(), &request.url);
    for (name, value) in &request.headers {
        let lower = name.to_ascii_lowercase();
        let credential = matches!(lower.as_str(), "cookie" | "authorization");
        if matches!(lower.as_str(), "user-agent" | "accept-language") || (credential && same_site) {
            builder = builder.header(name.as_str(), value.as_str());
        }
    }
    let mut response = builder.send().await?;
    if !response.status().is_success() {
        return Err(Error::from_status(response.status().as_u16(), None));
    }
    let content_type = response.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    if !content_type.is_empty() && !content_type.contains("html") && !content_type.contains("xml") {
        // A file, not a page: nothing to read links from.
        return Ok(None);
    }
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > MAX_PAGE_BYTES {
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Reads a page, and the same-site pages it links to down to
/// `options.depth`, collecting everything they point to.
///
/// The first page failing is an error; pages further down that fail are
/// skipped.
pub async fn explore(client: &reqwest::Client, request: &Request, options: GrabOptions, progress: &GrabProgress, cancel: &CancellationToken) -> Result<LinkBatch> {
    let start = url::Url::parse(request.url.trim()).map_err(|e| Error::InvalidUrl(format!("{}: {e}", request.url.trim())))?;
    if !matches!(start.scheme(), "http" | "https") {
        return Err(Error::InvalidUrl("only web pages can be explored".into()));
    }
    let site = site_of(&start);
    let depth_limit = options.depth.min(MAX_DEPTH);
    let page_limit = options.max_pages.clamp(1, MAX_PAGES);

    let mut batch = LinkBatch { page_url: start.to_string(), headers: request.headers.clone(), ..Default::default() };
    let mut seen_links: HashSet<String> = HashSet::new();
    let mut seen_pages: HashSet<String> = HashSet::from([start.to_string()]);
    let mut queue: VecDeque<(url::Url, u8)> = VecDeque::from([(start.clone(), 0)]);
    let mut running = tokio::task::JoinSet::new();
    let mut first = true;
    progress.pages_known.store(1, Ordering::Relaxed);

    loop {
        while running.len() < CONCURRENCY
            && let Some((url, depth)) = queue.pop_front()
        {
            let client = client.clone();
            let request = request.clone();
            running.spawn(async move {
                let page = fetch_page(&client, &request, &url).await;
                (url, depth, page)
            });
        }
        let joined = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            joined = running.join_next() => joined,
        };
        let Some(joined) = joined else { break };
        let Ok((url, depth, page)) = joined else { continue };
        progress.pages_read.fetch_add(1, Ordering::Relaxed);

        let html = match page {
            Ok(Some(html)) => html,
            Ok(None) if first => return Err(Error::Unsupported("That address is a file, not a page with links.".into())),
            Err(e) if first => return Err(e),
            _ => continue,
        };
        let found = links_in(&html, &url);
        if first {
            first = false;
            batch.page_title = found.title;
        }
        for link in found.links {
            if link.kind == LinkKind::Page {
                let on_site = url::Url::parse(&link.url).ok().as_ref().and_then(site_of) == site;
                if on_site
                    && depth < depth_limit
                    && seen_pages.len() < page_limit
                    && seen_pages.insert(link.url.clone())
                    && let Ok(next) = url::Url::parse(&link.url)
                {
                    queue.push_back((next, depth + 1));
                    progress.pages_known.fetch_add(1, Ordering::Relaxed);
                }
            }
            if batch.links.len() < MAX_LINKS && seen_links.insert(link.url.clone()) {
                batch.links.push(link);
                progress.links_found.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(html: &str) -> PageLinks {
        links_in(html, &url::Url::parse("https://www.example.com/files/index.html").unwrap())
    }

    #[test]
    fn links_are_resolved_classified_and_deduplicated() {
        let found = page(
            r##"<html><head><title>  Release
                 files </title></head><body>
                 <a href="setup-1.2.dmg">Installer <b>for macOS</b></a>
                 <a href="/docs/guide.pdf#page=3">Guide</a>
                 <a href="/docs/guide.pdf">Guide again</a>
                 <a href="about.html">About</a>
                 <a href="/download?id=7" download>Latest build</a>
                 <a href="#top">Top</a>
                 <a href="mailto:hi@example.com">Mail</a>
                 <a href="javascript:void(0)">Nothing</a>
                 <a href="https://other.example/x.zip" title="Mirror"></a>
                 <img src="shot.png" alt="A screenshot">
                 <img src="data:image/png;base64,AAAA" data-src="//cdn.example.com/lazy.jpg">
                 <video poster="p.jpg"><source src="clip.mp4"></video>
               </body></html>"##,
        );
        assert_eq!(found.title, "Release files");
        let by_url = |needle: &str| found.links.iter().find(|l| l.url.ends_with(needle)).unwrap_or_else(|| panic!("{needle} missing"));

        assert_eq!(by_url("setup-1.2.dmg").url, "https://www.example.com/files/setup-1.2.dmg");
        assert_eq!(by_url("setup-1.2.dmg").text, "Installer for macOS");
        assert_eq!(by_url("setup-1.2.dmg").kind, LinkKind::File);
        // The fragment is dropped, so the second link is the same address.
        assert_eq!(found.links.iter().filter(|l| l.url.ends_with("guide.pdf")).count(), 1);
        assert_eq!(by_url("about.html").kind, LinkKind::Page);
        // No extension, but the page says it is a download.
        assert_eq!(by_url("download?id=7").kind, LinkKind::File);
        assert_eq!(by_url("x.zip").text, "Mirror");
        let pictured = page(r#"<a href="full/ridge.jpg"><img src="thumb/ridge.webp" alt="  The ridge   at dawn "></a>"#);
        assert_eq!(pictured.links[0].text, "The ridge at dawn");
        assert_eq!(by_url("shot.png").kind, LinkKind::Embedded);
        assert_eq!(by_url("shot.png").text, "A screenshot");
        assert_eq!(by_url("lazy.jpg").url, "https://cdn.example.com/lazy.jpg");
        assert_eq!(by_url("clip.mp4").kind, LinkKind::Embedded);
        assert!(!found.links.iter().any(|l| l.url.contains("mailto") || l.url.contains("javascript") || l.url.contains("data:")));
        assert!(!found.links.iter().any(|l| l.url.ends_with("#top") || l.url.ends_with("index.html")));
    }

    #[test]
    fn base_href_and_srcset() {
        let found = page(r#"<base href="https://static.example.net/assets/"><a href="pack.7z">Pack</a><img srcset="s.jpg 480w, m.jpg 960w, l.jpg 1920w">"#);
        assert_eq!(found.links[0].url, "https://static.example.net/assets/pack.7z");
        assert_eq!(found.links[1].url, "https://static.example.net/assets/l.jpg");
    }

    #[test]
    fn broken_markup_still_yields_links() {
        let found = page("<a href='one.zip'>one<a href=two.zip>two</p></div><A HREF=\"THREE.ZIP\">three");
        let urls: Vec<&str> = found.links.iter().map(|l| l.url.rsplit('/').next().unwrap()).collect();
        assert_eq!(urls, vec!["one.zip", "two.zip", "THREE.ZIP"]);
        assert!(page("").links.is_empty());
        assert!(page("just text, no markup at all").links.is_empty());
    }

    #[test]
    fn cookies_stay_on_their_site() {
        let batch = LinkBatch {
            page_url: "https://www.example.com/files/".into(),
            headers: vec![("Cookie".into(), "sid=1".into()), ("User-Agent".into(), "Test".into())],
            ..Default::default()
        };
        let own = batch.request_for("https://example.com/a.zip");
        assert_eq!(own.header("cookie"), Some("sid=1"));
        assert_eq!(own.referer.as_deref(), Some("https://www.example.com/files/"));
        let foreign = batch.request_for("https://cdn.other.net/a.zip");
        assert_eq!(foreign.header("cookie"), None);
        assert_eq!(foreign.header("user-agent"), Some("Test"));
    }
}
