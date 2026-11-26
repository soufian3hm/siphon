//! User preferences, persisted as JSON in the data directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::disk::write_atomic;
use crate::net::NetConfig;
use crate::organize::{self, CategoryRule, SiteRule};
use crate::schedule::Schedule;
use crate::types::Category;

pub const MAX_CONNECTIONS: usize = 32;
pub const MAX_ACTIVE: usize = 12;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Where files go unless a download says otherwise.
    pub download_dir: PathBuf,
    /// When false, a captured link is shown first and waits for the user to
    /// confirm name and location. When true it starts immediately in
    /// `download_dir`.
    pub save_automatically: bool,
    /// Downloads running at the same time; the rest wait in the queue.
    pub max_active: usize,
    /// Connections per download.
    pub connections: usize,
    /// Bytes per second across all downloads; zero is unlimited.
    pub speed_limit: u64,
    /// Whether `speed_limit` is applied. Kept separate so the number survives
    /// being switched off.
    pub speed_limit_enabled: bool,
    /// Continue interrupted downloads when the application starts.
    pub resume_on_launch: bool,
    /// Try again by itself after a network failure.
    pub auto_retry: bool,
    /// Bring the window forward when the browser hands over a link.
    pub focus_on_capture: bool,
    /// Begin fetching a captured link while its confirmation is still on
    /// screen. By the time the user has chosen a folder, part of the file
    /// (often all of it) is already there.
    pub start_immediately: bool,
    /// File each download into a folder for its family (Video, Music, ...).
    pub sort_by_type: bool,
    /// One entry per family: its folder and any extra extensions.
    pub categories: Vec<CategoryRule>,
    /// Per-site connection limits and logins.
    pub sites: Vec<SiteRule>,
    pub schedule: Schedule,
    /// Keep the computer from idle-sleeping while downloads are running.
    pub keep_awake: bool,
    /// Label finished files as downloaded from the internet (quarantine and
    /// source address on macOS), as a browser does.
    pub mark_downloaded: bool,
    /// Give finished files the modification date the server reported.
    pub keep_server_date: bool,
    /// Command run for every finished file; `[File]` stands for its path.
    /// Empty runs nothing.
    pub on_complete: String,
    /// Show a card with "Open" and "Show" when a download finishes.
    pub notify_on_complete: bool,
    /// The picture height chosen first for video pages; a page that does not
    /// have it gets the best one below.
    pub video_quality: u32,
    /// A video page that refuses to be read logged out is read again with
    /// the cookies the browser handed over for it. Never for YouTube.
    pub page_login: bool,
    /// The browser setup screen has been shown once.
    pub browser_setup_seen: bool,
    pub net: NetConfig,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: default_download_dir(),
            save_automatically: false,
            max_active: 3,
            connections: 8,
            speed_limit: 2 * 1024 * 1024,
            speed_limit_enabled: false,
            resume_on_launch: true,
            auto_retry: true,
            focus_on_capture: true,
            start_immediately: true,
            sort_by_type: false,
            categories: organize::default_rules(),
            sites: Vec::new(),
            schedule: Schedule::default(),
            keep_awake: true,
            mark_downloaded: true,
            keep_server_date: false,
            on_complete: String::new(),
            notify_on_complete: true,
            video_quality: 1080,
            page_login: true,
            browser_setup_seen: false,
            net: NetConfig::default(),
        }
    }
}

impl Settings {
    /// Clamps every number into its supported range.
    pub fn sanitized(mut self) -> Self {
        self.max_active = self.max_active.clamp(1, MAX_ACTIVE);
        self.connections = self.connections.clamp(1, MAX_CONNECTIONS);
        if self.download_dir.as_os_str().is_empty() {
            self.download_dir = default_download_dir();
        }
        self.categories = organize::normalize_rules(self.categories);
        for site in &mut self.sites {
            site.pattern = organize::normalize_pattern(&site.pattern);
            site.username = site.username.trim().to_owned();
            site.connections = site.connections.map(|c| c.clamp(1, MAX_CONNECTIONS));
        }
        self.sites.retain(|site| !site.is_inert());
        self.schedule = self.schedule.sanitized();
        self.on_complete = self.on_complete.trim().to_owned();
        self.video_quality = self.video_quality.clamp(144, 4320);
        self
    }

    /// The family of a file, honouring the user's extra extensions.
    pub fn category_of(&self, file_name: &str) -> Category {
        organize::category_of(&self.categories, file_name)
    }

    /// The folder a family is sorted into when sorting is on.
    pub fn category_folder(&self, category: Category) -> PathBuf {
        match self.categories.iter().find(|r| r.category == category).and_then(|r| r.folder.clone()) {
            Some(folder) => folder,
            None => match category.folder_name() {
                Some(name) => self.download_dir.join(name),
                None => self.download_dir.clone(),
            },
        }
    }

    /// Where a file of this name goes by default.
    pub fn folder_for(&self, file_name: &str) -> PathBuf {
        if self.sort_by_type { self.category_folder(self.category_of(file_name)) } else { self.download_dir.clone() }
    }

    /// The first site rule that covers `host`.
    pub fn site_rule(&self, host: &str) -> Option<&SiteRule> {
        self.sites.iter().find(|rule| rule.matches(host))
    }

    /// The limit workers actually obey.
    pub fn effective_speed_limit(&self) -> u64 {
        if self.speed_limit_enabled { self.speed_limit } else { 0 }
    }

    /// Loads settings, falling back to defaults for a missing or unreadable
    /// file and for any field an older version did not write.
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Settings>(&bytes).ok())
            .unwrap_or_default()
            .sanitized()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        write_atomic(path, &serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)
    }
}

/// The user's Downloads folder, or the home directory if the platform does
/// not define one.
pub fn default_download_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(Path::to_owned).or_else(|| Some(dirs.home_dir().join("Downloads"))))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Where Siphon keeps its own state (download list, settings, IPC endpoint).
pub fn default_data_dir() -> PathBuf {
    directories::ProjectDirs::from("app", "Siphon", "Siphon")
        .map(|dirs| dirs.data_dir().to_owned())
        .unwrap_or_else(|| PathBuf::from(".siphon"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_old_files() {
        let dir = std::env::temp_dir().join(format!("siphon-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(Settings::load(&path), Settings::default());

        let mut s = Settings { save_automatically: true, connections: 16, ..Default::default() };
        s.net.proxy.host = "proxy.corp".into();
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);

        // A file written before most options existed, with a value out of range.
        std::fs::write(&path, br#"{"connections": 500, "save_automatically": true}"#).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.connections, MAX_CONNECTIONS);
        assert!(loaded.save_automatically);
        assert_eq!(loaded.max_active, Settings::default().max_active);

        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn files_are_sorted_only_when_asked() {
        let mut s = Settings { download_dir: PathBuf::from("/dl"), ..Default::default() };
        assert_eq!(s.folder_for("movie.mkv"), PathBuf::from("/dl"));
        s.sort_by_type = true;
        assert_eq!(s.folder_for("movie.mkv"), PathBuf::from("/dl/Video"));
        assert_eq!(s.folder_for("song.flac"), PathBuf::from("/dl/Music"));
        assert_eq!(s.folder_for("mystery"), PathBuf::from("/dl"));
        s.categories[0].folder = Some(PathBuf::from("/Volumes/Media/Films"));
        assert_eq!(s.folder_for("movie.mkv"), PathBuf::from("/Volumes/Media/Films"));
    }

    #[test]
    fn site_rules_are_tidied_and_matched_in_order() {
        let s = Settings {
            sites: vec![
                SiteRule { pattern: " https://Files.Example.com/pub/ ".into(), connections: Some(999), username: " ada ".into() },
                SiteRule { pattern: "example.com".into(), connections: Some(2), username: String::new() },
                SiteRule { pattern: "nothing.example".into(), connections: None, username: String::new() },
            ],
            ..Default::default()
        }
        .sanitized();
        assert_eq!(s.sites.len(), 2);
        assert_eq!(s.sites[0].pattern, "files.example.com");
        assert_eq!(s.sites[0].connections, Some(MAX_CONNECTIONS));
        assert_eq!(s.sites[0].username, "ada");
        assert_eq!(s.site_rule("files.example.com").unwrap().username, "ada");
        assert_eq!(s.site_rule("cdn.example.com").unwrap().connections, Some(2));
        assert!(s.site_rule("example.org").is_none());
    }

    #[test]
    fn the_limit_only_applies_when_enabled() {
        let off = Settings::default();
        assert_eq!(off.effective_speed_limit(), 0);
        let on = Settings { speed_limit_enabled: true, ..Default::default() };
        assert_eq!(on.effective_speed_limit(), on.speed_limit);
    }
}
