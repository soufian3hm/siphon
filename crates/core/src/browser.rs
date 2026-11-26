//! Registering the native-messaging host with the browsers on this machine.
//!
//! A Chromium-family browser will only start a native host that is described
//! by a manifest in a location the browser trusts, and only for the
//! extension IDs that manifest lists. The extension ships with a fixed key,
//! so its ID is known here and no user ever has to paste one.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::disk::write_atomic;

/// ID of the Siphon extension, fixed by the public key in its manifest.
pub const EXTENSION_ID: &str = "epjmjlofaaebaippnkglkkifcdgpgmfp";
/// Name the extension passes to `chrome.runtime.connectNative`.
pub const HOST_NAME: &str = "app.siphon.host";
/// ID of the extension's Chrome Web Store listing, once it has one.
///
/// On macOS and Windows a browser only accepts an extension from an
/// installer when it is hosted in the Chrome Web Store: the installer leaves
/// a small pointer file, the browser fetches the extension from the store
/// and asks the user to switch it on. Until the extension is published this
/// is `None`, the pointer is not written, and the extension is added by hand
/// from its folder.
pub const WEB_STORE_ID: Option<&str> = None;
/// Where browsers fetch store extensions from.
const WEB_STORE_UPDATE_URL: &str = "https://clients2.google.com/service/update2/crx";

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct HostManifest {
    name: String,
    description: String,
    path: String,
    #[serde(rename = "type")]
    kind: String,
    allowed_origins: Vec<String>,
}

fn manifest_for(host_binary: &Path) -> HostManifest {
    HostManifest {
        name: HOST_NAME.to_owned(),
        description: "Siphon download manager".to_owned(),
        path: host_binary.to_string_lossy().into_owned(),
        kind: "stdio".to_owned(),
        // The copy loaded from its folder and the copy from the store have
        // different IDs; both may talk to the helper.
        allowed_origins: std::iter::once(EXTENSION_ID).chain(WEB_STORE_ID).map(|id| format!("chrome-extension://{id}/")).collect(),
    }
}

/// A browser that can run the extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Browser {
    pub name: &'static str,
    /// The browser's profile root.
    pub root: PathBuf,
    /// The application's name on disk (`Google Chrome` for
    /// `Google Chrome.app`).
    pub app: &'static str,
    #[cfg(windows)]
    registry_key: &'static str,
}

impl Browser {
    /// The browser has a profile folder: it has been run on this account.
    pub fn is_installed(&self) -> bool {
        self.root.is_dir()
    }

    /// Where the browser's application is, if it is on this computer.
    pub fn application(&self) -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().join("Applications"));
            std::iter::once(PathBuf::from("/Applications")).chain(home).map(|dir| dir.join(format!("{}.app", self.app))).find(|path| path.is_dir())
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    /// The browser can be used here: its application is installed (macOS),
    /// or it has a profile folder (elsewhere).
    pub fn is_present(&self) -> bool {
        if cfg!(target_os = "macos") { self.application().is_some() } else { self.is_installed() }
    }

    /// Whether the Siphon extension has been added to one of this browser's
    /// profiles. Read from the profile's own record of its extensions.
    pub fn has_extension(&self) -> bool {
        let Ok(entries) = std::fs::read_dir(&self.root) else { return false };
        let ids: Vec<&str> = std::iter::once(EXTENSION_ID).chain(WEB_STORE_ID).collect();
        entries.flatten().filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name == "Default" || name.starts_with("Profile ")
        }).any(|profile| {
            ["Secure Preferences", "Preferences"].iter().any(|file| {
                let path = profile.path().join(file);
                // A profile's settings are a few megabytes at most.
                let small = std::fs::metadata(&path).is_ok_and(|meta| meta.len() < 64 * 1024 * 1024);
                small && std::fs::read(&path).is_ok_and(|bytes| ids.iter().any(|id| contains(&bytes, format!("\"{id}\"").as_bytes())))
            })
        })
    }

    /// Opens the browser on its page of extensions.
    pub fn open_extensions_page(&self) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            let app = self.application().ok_or_else(|| std::io::Error::other(format!("{} is not installed", self.name)))?;
            let status = std::process::Command::new("/usr/bin/open").arg("-a").arg(app).arg("chrome://extensions/").status()?;
            if status.success() { Ok(()) } else { Err(std::io::Error::other(format!("{} did not open its extensions page", self.name))) }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(std::io::Error::other("open the browser's extensions page by hand"))
        }
    }

    fn store_pointer(&self, store_id: &str) -> PathBuf {
        self.root.join("External Extensions").join(format!("{store_id}.json"))
    }

    /// Tells the browser to fetch the extension from the Chrome Web Store.
    /// At its next start the browser installs it and asks the user to switch
    /// it on. This is the only route by which an installer may add an
    /// extension on macOS and Windows.
    pub fn add_from_store(&self, store_id: &str) -> std::io::Result<PathBuf> {
        let path = self.store_pointer(store_id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let pointer = serde_json::json!({ "external_update_url": WEB_STORE_UPDATE_URL });
        write_atomic(&path, &serde_json::to_vec_pretty(&pointer).map_err(std::io::Error::other)?)?;
        Ok(path)
    }

    /// Takes the pointer away again; the browser then removes the extension.
    pub fn remove_from_store(&self, store_id: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.store_pointer(store_id)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join("NativeMessagingHosts").join(format!("{HOST_NAME}.json"))
    }

    /// The host this browser would start, if one is registered.
    pub fn registered_host(&self) -> Option<PathBuf> {
        let manifest: HostManifest = serde_json::from_slice(&std::fs::read(self.manifest_path()).ok()?).ok()?;
        (manifest.name == HOST_NAME).then(|| PathBuf::from(manifest.path))
    }

    fn register(&self, host_binary: &Path) -> std::io::Result<PathBuf> {
        let path = self.manifest_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(&manifest_for(host_binary)).map_err(std::io::Error::other)?;
        write_atomic(&path, &json)?;
        #[cfg(windows)]
        {
            // Windows browsers find the manifest through the registry.
            let key = format!("HKCU\\Software\\{}\\NativeMessagingHosts\\{HOST_NAME}", self.registry_key);
            let status = std::process::Command::new("reg")
                .args(["add", &key, "/ve", "/t", "REG_SZ", "/d"])
                .arg(&path)
                .arg("/f")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?;
            if !status.success() {
                return Err(std::io::Error::other("could not write the registry entry"));
            }
        }
        Ok(path)
    }
}

/// Every Chromium-family browser this build knows how to register with.
pub fn browsers() -> Vec<Browser> {
    let Some(dirs) = directories::BaseDirs::new() else { return Vec::new() };

    #[cfg(target_os = "macos")]
    let (base, list): (PathBuf, &[(&str, &str, &str)]) = (
        dirs.home_dir().join("Library/Application Support"),
        &[
            ("Google Chrome", "Google/Chrome", "Google Chrome"),
            ("Chrome Beta", "Google/Chrome Beta", "Google Chrome Beta"),
            ("Chrome Canary", "Google/Chrome Canary", "Google Chrome Canary"),
            ("Dia", "Dia/User Data", "Dia"),
            ("Brave", "BraveSoftware/Brave-Browser", "Brave Browser"),
            ("Microsoft Edge", "Microsoft Edge", "Microsoft Edge"),
            ("Arc", "Arc/User Data", "Arc"),
            ("Vivaldi", "Vivaldi", "Vivaldi"),
            ("Opera", "com.operasoftware.Opera", "Opera"),
            ("Chromium", "Chromium", "Chromium"),
        ],
    );
    #[cfg(all(unix, not(target_os = "macos")))]
    let (base, list): (PathBuf, &[(&str, &str, &str)]) = (
        dirs.config_dir().to_owned(),
        &[
            ("Google Chrome", "google-chrome", "google-chrome"),
            ("Chrome Beta", "google-chrome-beta", "google-chrome-beta"),
            ("Chromium", "chromium", "chromium"),
            ("Microsoft Edge", "microsoft-edge", "microsoft-edge"),
            ("Brave", "BraveSoftware/Brave-Browser", "brave-browser"),
            ("Vivaldi", "vivaldi", "vivaldi"),
        ],
    );
    #[cfg(windows)]
    let (base, list): (PathBuf, &[(&str, &str, &str)]) = (
        dirs.data_local_dir().to_owned(),
        &[
            ("Google Chrome", "Google/Chrome/User Data", "Google\\Chrome"),
            ("Chromium", "Chromium/User Data", "Chromium"),
            ("Microsoft Edge", "Microsoft/Edge/User Data", "Microsoft\\Edge"),
            ("Brave", "BraveSoftware/Brave-Browser/User Data", "BraveSoftware\\Brave-Browser"),
            ("Vivaldi", "Vivaldi/User Data", "Vivaldi"),
        ],
    );

    #[cfg(not(windows))]
    return list.iter().map(|(name, rel, app)| Browser { name, root: base.join(rel), app }).collect();
    #[cfg(windows)]
    return list.iter().map(|(name, rel, key)| Browser { name, root: base.join(rel), app: name, registry_key: key }).collect();
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|window| window == needle)
}

/// The browsers on this computer, in the order they are listed to the user.
pub fn present() -> Vec<Browser> {
    browsers().into_iter().filter(Browser::is_present).collect()
}

/// Keeps a copy of the extension where a browser can load it from and the
/// user can find it: a plain folder that does not move when the application
/// does. Refreshed whenever the bundled extension is a different version.
pub fn sync_extension(bundled: &Path, data_dir: &Path) -> std::io::Result<PathBuf> {
    fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), target)?;
            }
        }
        Ok(())
    }
    let target = data_dir.join("Siphon Extension");
    let fresh = std::fs::read(bundled.join("manifest.json"))?;
    // Every file is compared by the one that names the version, plus the
    // code, so a development build with the same version still refreshes.
    let same = |name: &str| std::fs::read(bundled.join(name)).ok() == std::fs::read(target.join(name)).ok();
    if std::fs::read(target.join("manifest.json")).ok().as_deref() == Some(fresh.as_slice()) && same("background.js") && same("content.js") && same("popup.js") {
        return Ok(target);
    }
    // Overwritten in place: a browser that has it loaded keeps working and
    // picks the new files up the next time it starts.
    copy(bundled, &target)?;
    Ok(target)
}

/// How the registration went for one browser.
#[derive(Debug)]
pub struct Registration {
    pub browser: &'static str,
    pub result: std::io::Result<PathBuf>,
}

/// Registers `host_binary` with every installed browser.
pub fn register_host(host_binary: &Path) -> Vec<Registration> {
    browsers()
        .into_iter()
        .filter(|browser| browser.is_installed() || browser.is_present())
        .map(|browser| Registration { browser: browser.name, result: browser.register(host_binary) })
        .collect()
}

/// Installed browsers that would start exactly this host binary.
pub fn browsers_registered_with(host_binary: &Path) -> Vec<&'static str> {
    browsers()
        .into_iter()
        .filter(|b| b.is_installed() && b.registered_host().as_deref() == Some(host_binary))
        .map(|b| b.name)
        .collect()
}

/// The host binary that ships next to the running executable.
pub fn bundled_host() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?.canonicalize().ok()?;
    let name = if cfg!(windows) { "siphon-host.exe" } else { "siphon-host" };
    let host = me.parent()?.join(name);
    host.is_file().then_some(host)
}

/// The application bundle an executable is the main program of, if it is
/// inside one (`Siphon.app/Contents/MacOS/siphon`).
pub fn bundle_of(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && bundle.extension().is_some_and(|e| e == "app")).then(|| bundle.to_owned())
}

/// Brings a running Siphon to the front through the operating system.
///
/// On macOS a window that is covered, on another desktop or in the Dock is
/// not redrawn, so the application cannot do this from its own frame loop:
/// the request would wait until the user happened to look. Two routes:
///
/// * Opening the application's bundle. The system finds the running copy,
///   activates it and restores its window if it was minimised. Right for an
///   installed copy that the system itself started.
/// * Activating the process by its id. Used for scratch runs (a test or a
///   development build), where opening the bundle would address, or even
///   start, the user's real copy instead. It does not restore a minimised
///   window.
///
/// `SIPHON_ATTENTION=open|pid` forces a route. Never blocks: when the caller
/// is the application's own main thread, waiting would be waiting for itself.
pub fn bring_forward(pid: u32, bundle: Option<&Path>) {
    #[cfg(target_os = "macos")]
    {
        let scratch = std::env::var_os("SIPHON_DATA_DIR").is_some();
        let forced = std::env::var("SIPHON_ATTENTION").ok();
        let by_bundle = match forced.as_deref() {
            Some("open") => bundle.is_some(),
            Some("pid") => false,
            _ => bundle.is_some() && !scratch,
        };
        let mut command = if by_bundle {
            let mut open = std::process::Command::new("/usr/bin/open");
            open.arg(bundle.expect("checked above"));
            open
        } else {
            // 3 = all windows, even over the application in front.
            let script = format!("ObjC.import('AppKit'); $.NSRunningApplication.runningApplicationWithProcessIdentifier({pid}).activateWithOptions(3);");
            let mut osascript = std::process::Command::new("/usr/bin/osascript");
            osascript.args(["-l", "JavaScript", "-e", &script]);
            osascript
        };
        command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        std::thread::spawn(move || {
            let _ = command.status();
        });
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (pid, bundle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_names_the_host_and_allows_only_our_extension() {
        let manifest = manifest_for(Path::new("/Applications/Siphon.app/Contents/MacOS/siphon-host"));
        let json = serde_json::to_value(&manifest).unwrap();
        assert_eq!(json["name"], "app.siphon.host");
        assert_eq!(json["type"], "stdio");
        assert_eq!(json["path"], "/Applications/Siphon.app/Contents/MacOS/siphon-host");
        assert_eq!(json["allowed_origins"], serde_json::json!(["chrome-extension://epjmjlofaaebaippnkglkkifcdgpgmfp/"]));
    }

    #[test]
    fn the_extension_id_has_the_shape_chrome_expects() {
        assert_eq!(EXTENSION_ID.len(), 32);
        assert!(EXTENSION_ID.bytes().all(|b| (b'a'..=b'p').contains(&b)));
        // Host names: lowercase alphanumerics, underscores and dots only.
        assert!(HOST_NAME.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_'));
    }

    #[cfg(not(windows))]
    #[test]
    fn registering_writes_a_manifest_the_browser_can_read_back() {
        let root = std::env::temp_dir().join(format!("siphon-browser-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let browser = Browser { name: "Test", root: root.clone(), app: "Test" };
        assert!(browser.is_installed());
        assert_eq!(browser.registered_host(), None);

        let host = Path::new("/opt/siphon/siphon-host");
        let written = browser.register(host).unwrap();
        assert_eq!(written, root.join("NativeMessagingHosts/app.siphon.host.json"));
        assert_eq!(browser.registered_host().as_deref(), Some(host));

        // Re-registering after the application moved replaces the path.
        let moved = Path::new("/Applications/Siphon.app/Contents/MacOS/siphon-host");
        browser.register(moved).unwrap();
        assert_eq!(browser.registered_host().as_deref(), Some(moved));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn the_extension_is_found_in_a_profile_and_the_store_pointer_is_what_browsers_read() {
        let root = std::env::temp_dir().join(format!("siphon-browser-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let browser = Browser { name: "Test", root: root.clone(), app: "No Such Browser" };
        assert!(!browser.has_extension(), "no profile folder at all");
        assert!(browser.application().is_none());

        // A profile that knows other extensions only.
        std::fs::create_dir_all(root.join("Default")).unwrap();
        std::fs::write(root.join("Default/Secure Preferences"), br#"{"extensions":{"settings":{"aaaabbbbccccddddeeeeffffgggghhhh":{"state":1}}}}"#).unwrap();
        assert!(!browser.has_extension());
        // A second profile has ours.
        std::fs::create_dir_all(root.join("Profile 2")).unwrap();
        std::fs::write(root.join("Profile 2/Secure Preferences"), format!(r#"{{"extensions":{{"settings":{{"{EXTENSION_ID}":{{"path":"/x","state":1}}}}}}}}"#)).unwrap();
        assert!(browser.has_extension());
        // Folders that are not profiles are not looked into.
        std::fs::remove_dir_all(root.join("Profile 2")).unwrap();
        std::fs::create_dir_all(root.join("Crashpad")).unwrap();
        std::fs::write(root.join("Crashpad/Preferences"), format!("\"{EXTENSION_ID}\"")).unwrap();
        assert!(!browser.has_extension());

        let store_id = "abcdefghijklmnopabcdefghijklmnop";
        let pointer = browser.add_from_store(store_id).unwrap();
        assert_eq!(pointer, root.join("External Extensions/abcdefghijklmnopabcdefghijklmnop.json"));
        let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&pointer).unwrap()).unwrap();
        assert_eq!(json, serde_json::json!({ "external_update_url": "https://clients2.google.com/service/update2/crx" }));
        browser.remove_from_store(store_id).unwrap();
        assert!(!pointer.exists());
        browser.remove_from_store(store_id).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_extension_folder_is_copied_once_and_refreshed_when_it_changes() {
        let root = std::env::temp_dir().join(format!("siphon-browser-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (bundled, data) = (root.join("bundle/extension"), root.join("data"));
        std::fs::create_dir_all(bundled.join("icons")).unwrap();
        for (name, text) in [("manifest.json", r#"{"version":"1"}"#), ("background.js", "a"), ("content.js", "b"), ("popup.js", "c"), ("icons/16.png", "png")] {
            std::fs::write(bundled.join(name), text).unwrap();
        }
        let copy = sync_extension(&bundled, &data).unwrap();
        assert_eq!(copy, data.join("Siphon Extension"));
        assert_eq!(std::fs::read_to_string(copy.join("icons/16.png")).unwrap(), "png");

        // Unchanged: left alone (a marker written into the copy survives).
        std::fs::write(copy.join("marker"), "x").unwrap();
        std::fs::write(copy.join("icons/16.png"), "edited").unwrap();
        sync_extension(&bundled, &data).unwrap();
        assert_eq!(std::fs::read_to_string(copy.join("icons/16.png")).unwrap(), "edited");

        // A new version, or new code under the same version: refreshed.
        std::fs::write(bundled.join("content.js"), "b2").unwrap();
        sync_extension(&bundled, &data).unwrap();
        assert_eq!(std::fs::read_to_string(copy.join("content.js")).unwrap(), "b2");
        assert_eq!(std::fs::read_to_string(copy.join("icons/16.png")).unwrap(), "png");
        assert!(sync_extension(&root.join("missing"), &data).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
