//! Getting Siphon into place on a computer: into the Applications folder,
//! and connected to the browsers that are installed.

use std::path::{Path, PathBuf};

use siphon_core::browser;

/// The application bundle this process runs from, if it runs from one.
pub fn bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    browser::bundle_of(&exe)
}

/// Whether this copy runs from somewhere it should not stay: a mounted disk
/// image, or the read-only scratch location macOS runs a freshly downloaded
/// application from. Browsers are given the helper's path, so the
/// application has to live where that path keeps meaning something.
pub fn needs_installing() -> bool {
    let Some(bundle) = bundle() else { return false };
    if bundle.components().any(|part| part.as_os_str() == "AppTranslocation") {
        return true;
    }
    // A mounted volume that cannot be written to is a disk image (or a disc).
    bundle.starts_with("/Volumes") && bundle.parent().is_some_and(|dir| {
        let probe = dir.join(format!(".siphon-write-test-{}", std::process::id()));
        match std::fs::write(&probe, b"") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
                false
            }
            Err(_) => true,
        }
    })
}

/// Copies this application into the Applications folder and returns where it
/// landed. A copy already there is replaced, having been set aside until the
/// new one is whole.
pub fn move_to_applications() -> Result<PathBuf, String> {
    install(None)
}

/// Copies this application into `folder`, or, given none, into the
/// Applications folder: the shared one, else the account's own.
pub fn install(folder: Option<&Path>) -> Result<PathBuf, String> {
    let source = bundle().ok_or("Siphon is not running from an application bundle")?;
    let name = source.file_name().ok_or("the application has no name")?.to_owned();
    let folders: Vec<PathBuf> = match folder {
        Some(folder) => vec![folder.to_owned()],
        // The shared folder first; an account without rights to it has its own.
        None => std::iter::once(PathBuf::from("/Applications")).chain(std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Applications"))).collect(),
    };
    let mut last_error = String::from("no Applications folder could be written to");
    for folder in folders {
        match install_into(&source, &folder.join(&name)) {
            Ok(target) => return Ok(target),
            Err(e) => last_error = format!("{}: {e}", folder.display()),
        }
    }
    Err(last_error)
}

fn install_into(source: &Path, target: &Path) -> std::io::Result<PathBuf> {
    if source == target {
        return Ok(target.to_owned());
    }
    if let Some(folder) = target.parent() {
        std::fs::create_dir_all(folder)?;
    }
    let incoming = target.with_extension("app.installing");
    let outgoing = target.with_extension("app.replaced");
    let _ = std::fs::remove_dir_all(&incoming);
    let _ = std::fs::remove_dir_all(&outgoing);
    // `ditto` copies a bundle whole: signatures, attributes, symbolic links.
    let status = std::process::Command::new("/usr/bin/ditto").arg(source).arg(&incoming).status()?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&incoming);
        return Err(std::io::Error::other("the copy did not complete"));
    }
    if target.exists() {
        std::fs::rename(target, &outgoing)?;
    }
    if let Err(e) = std::fs::rename(&incoming, target) {
        // Put the previous copy back rather than leave none.
        let _ = std::fs::rename(&outgoing, target);
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&outgoing);
    Ok(target.to_owned())
}

/// The folder a browser loads the extension from: the stable copy in the
/// data folder when there is one, else the one inside the application.
pub fn extension_folder(data_dir: &Path) -> Option<PathBuf> {
    let bundled = crate::dialogs::extension_dir()?;
    Some(browser::sync_extension(&bundled, data_dir).unwrap_or(bundled))
}

/// Makes every Chromium-family browser on this computer able to reach this
/// copy of Siphon, and brings the extension folder up to date.
///
/// What an installer may do about the extension itself depends on where the
/// extension is hosted. Published in the Chrome Web Store, a pointer is left
/// for each browser, which then fetches it and asks the user to switch it
/// on. Unpublished, a browser on macOS accepts it only from the user's own
/// hand, and this function leaves that to the setup screen.
pub fn connect_browsers(data_dir: &Path) {
    if let Some(host) = browser::bundled_host() {
        let stale = browser::browsers().iter().any(|b| (b.is_present() || b.is_installed()) && b.registered_host().as_deref() != Some(host.as_path()));
        if stale {
            browser::register_host(&host);
        }
    }
    let _ = extension_folder(data_dir);
    if let Some(store_id) = browser::WEB_STORE_ID {
        for found in browser::present() {
            let _ = found.add_from_store(store_id);
        }
    }
}

/// Starts the installed copy once this process is gone, and puts away the
/// disk image this one ran from. A second copy started while this one still
/// answers would only hand over to it and leave, so the start has to wait.
pub fn relaunch_after_exit(installed: &Path) -> std::io::Result<()> {
    // `/Volumes/<name>/Siphon.app`: the image is ours to eject.
    let image = bundle().filter(|bundle| bundle.starts_with("/Volumes") && bundle.components().count() == 4).and_then(|bundle| bundle.parent().map(Path::to_owned));
    let script = r#"n=0
while kill -0 "$1" 2>/dev/null && [ "$n" -lt 100 ]; do sleep 0.1; n=$((n+1)); done
/usr/bin/open "$2"
if [ -n "$3" ]; then /usr/bin/hdiutil detach "$3" -quiet; fi
exit 0"#;
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg("siphon-relaunch")
        .arg(std::process::id().to_string())
        .arg(installed)
        .arg(image.unwrap_or_default())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Shows a folder in the file manager, selected in the folder that holds it.
pub fn reveal(path: &Path) -> std::io::Result<()> {
    if cfg!(target_os = "macos") {
        std::process::Command::new("/usr/bin/open").arg("-R").arg(path).spawn().map(|_| ())
    } else {
        open::that_detached(path.parent().unwrap_or(path))
    }
}

/// One browser as the setup screen shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct BrowserState {
    pub browser: browser::Browser,
    /// The browser would start this copy's helper.
    pub connected: bool,
    /// The extension is in one of its profiles.
    pub extension: bool,
    /// The browser's own icon, as a picture file.
    pub icon: Option<PathBuf>,
}

/// Looks at every browser on this computer. Reads each profile's record of
/// its extensions, so it belongs on a background thread.
pub fn survey(data_dir: &Path) -> Vec<BrowserState> {
    let host = browser::bundled_host();
    browser::present()
        .into_iter()
        .map(|found| BrowserState {
            connected: host.is_some() && found.registered_host() == host,
            extension: found.has_extension(),
            icon: found.application().and_then(|app| icon_of(&app, data_dir, found.name)),
            browser: found,
        })
        .collect()
}

/// Draws an application's icon into a 128-pixel picture, the way the system
/// would show it, whatever format the application keeps it in.
const ICON_SCRIPT: &str = r#"ObjC.import('AppKit');
function run(argv) {
  var icon = $.NSWorkspace.sharedWorkspace.iconForFile(argv[0]);
  var side = 128;
  var rep = $.NSBitmapImageRep.alloc.initWithBitmapDataPlanesPixelsWidePixelsHighBitsPerSampleSamplesPerPixelHasAlphaIsPlanarColorSpaceNameBytesPerRowBitsPerPixel(null, side, side, 8, 4, true, false, $.NSDeviceRGBColorSpace, 0, 0);
  var context = $.NSGraphicsContext.graphicsContextWithBitmapImageRep(rep);
  $.NSGraphicsContext.saveGraphicsState;
  $.NSGraphicsContext.setCurrentContext(context);
  icon.drawInRectFromRectOperationFraction($.NSMakeRect(0, 0, side, side), $.NSZeroRect, $.NSCompositingOperationSourceOver, 1.0);
  context.flushGraphics;
  $.NSGraphicsContext.restoreGraphicsState;
  var png = rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $.NSDictionary.dictionary);
  return png.writeToFileAtomically(argv[1], true) ? 'ok' : 'failed';
}"#;

/// The picture of `application`'s icon, drawn once and kept until the
/// application changes.
fn icon_of(application: &Path, data_dir: &Path, name: &str) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let folder = data_dir.join("cache").join("icons");
    let picture = folder.join(format!("{name}.png"));
    let modified = |path: &Path| std::fs::metadata(path).and_then(|meta| meta.modified()).ok();
    if let (Some(drawn), Some(changed)) = (modified(&picture), modified(application))
        && drawn >= changed
    {
        return Some(picture);
    }
    std::fs::create_dir_all(&folder).ok()?;
    let output = std::process::Command::new("/usr/bin/osascript").args(["-l", "JavaScript", "-e", ICON_SCRIPT]).arg(application).arg(&picture).stdin(std::process::Stdio::null()).output().ok()?;
    (output.status.success() && picture.is_file()).then_some(picture)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bundle_is_installed_whole_and_replaces_the_copy_that_was_there() {
        let root = std::env::temp_dir().join(format!("siphon-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join("image/Siphon.app");
        std::fs::create_dir_all(source.join("Contents/MacOS")).unwrap();
        std::fs::write(source.join("Contents/MacOS/siphon"), b"new").unwrap();
        std::fs::write(source.join("Contents/Info.plist"), b"plist").unwrap();

        let target = root.join("Applications/Siphon.app");
        std::fs::create_dir_all(target.join("Contents/MacOS")).unwrap();
        std::fs::write(target.join("Contents/MacOS/siphon"), b"old").unwrap();
        std::fs::write(target.join("Contents/leftover"), b"x").unwrap();

        assert_eq!(install_into(&source, &target).unwrap(), target);
        assert_eq!(std::fs::read(target.join("Contents/MacOS/siphon")).unwrap(), b"new");
        assert!(!target.join("Contents/leftover").exists(), "the old copy is replaced, not merged into");
        assert!(source.exists(), "the source is copied, not moved: a disk image cannot be taken from");
        let around: Vec<String> = std::fs::read_dir(root.join("Applications")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(around, vec!["Siphon.app"], "nothing is left beside it");

        // Installing over itself changes nothing.
        assert_eq!(install_into(&target, &target).unwrap(), target);
        assert!(install_into(&root.join("missing.app"), &target).is_err());
        assert_eq!(std::fs::read(target.join("Contents/MacOS/siphon")).unwrap(), b"new", "a failed install leaves the working copy");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn an_applications_icon_is_drawn_once_and_kept() {
        let data = std::env::temp_dir().join(format!("siphon-icons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        // An application every Mac has.
        let finder = Path::new("/System/Library/CoreServices/Finder.app");
        let picture = icon_of(finder, &data, "Finder").expect("the icon is drawn");
        let image = image::open(&picture).unwrap().to_rgba8();
        assert_eq!((image.width(), image.height()), (128, 128));
        assert!(image.pixels().any(|pixel| pixel[3] == 255), "the picture has something in it");
        assert!(image.pixels().any(|pixel| pixel[3] == 0), "and transparent corners");
        let drawn = std::fs::metadata(&picture).unwrap().modified().unwrap();
        assert_eq!(icon_of(finder, &data, "Finder"), Some(picture.clone()));
        assert_eq!(std::fs::metadata(&picture).unwrap().modified().unwrap(), drawn, "the kept picture is used again");
        assert_eq!(icon_of(Path::new("/nonexistent/Nothing.app"), &data, "Nothing").map(|p| p.exists()), Some(true), "the system has a stand-in icon for anything");
        std::fs::remove_dir_all(data).unwrap();
    }
}
