//! Sandbox-safe storage and persistent access to files chosen in native panels.
use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from(objc2_foundation::NSHomeDirectory().to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    }
}

#[cfg(target_os = "macos")]
mod bookmarks {
    use super::*;
    use objc2::{rc::Retained, runtime::Bool};
    use objc2_foundation::{
        NSData, NSString, NSURLBookmarkCreationOptions as Create,
        NSURLBookmarkResolutionOptions as Resolve, NSURL,
    };
    use sha2::{Digest, Sha256};
    use std::cell::RefCell;

    struct Access(Retained<NSURL>);
    impl Drop for Access {
        fn drop(&mut self) {
            unsafe {
                self.0.stopAccessingSecurityScopedResource();
            }
        }
    }
    thread_local! { static ACTIVE: RefCell<Vec<(PathBuf, Access)>> = const { RefCell::new(Vec::new()) }; }
    fn location(path: &Path) -> PathBuf {
        let name = if crate::dev::is_dev() {
            "gravitynote-gpui-dev"
        } else {
            "gravitynote-gpui"
        };
        home()
            .join("Library/Application Support")
            .join(name)
            .join("bookmarks")
            .join(format!(
                "{:x}.bookmark",
                Sha256::digest(path.as_os_str().as_encoded_bytes())
            ))
    }
    pub fn remember(path: &Path) -> Result<(), String> {
        // Internal documents already have permanent container access.
        if path.starts_with(home().join("Library/Application Support")) {
            return Ok(());
        }
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let data = url
            .bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
                Create::WithSecurityScope,
                None,
                None,
            )
            .map_err(|e| e.to_string())?;
        let target = location(path);
        std::fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
        let tmp = target.with_extension("tmp");
        std::fs::write(&tmp, data.to_vec()).map_err(|e| e.to_string())?;
        std::fs::rename(tmp, target).map_err(|e| e.to_string())
    }
    pub fn restore(path: &Path) -> PathBuf {
        if let Some(found) = ACTIVE.with(|a| {
            a.borrow()
                .iter()
                .find(|(p, _)| p == path)
                .and_then(|(_, a)| a.0.path())
                .map(|p| PathBuf::from(p.to_string()))
        }) {
            return found;
        }
        let Ok(bytes) = std::fs::read(location(path)) else {
            return path.to_path_buf();
        };
        let data = NSData::with_bytes(&bytes);
        let mut stale = Bool::NO;
        let url = unsafe {
            NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
                &data,
                Resolve::WithSecurityScope | Resolve::WithoutUI,
                None,
                &mut stale,
            )
        };
        let Ok(url) = url else {
            return path.to_path_buf();
        };
        if !unsafe { url.startAccessingSecurityScopedResource() } {
            return path.to_path_buf();
        }
        let resolved = url
            .path()
            .map(|p| PathBuf::from(p.to_string()))
            .unwrap_or_else(|| path.to_path_buf());
        // Keep a balanced access token for the whole editing session, including autosave.
        ACTIVE.with(|a| a.borrow_mut().push((path.to_path_buf(), Access(url))));
        if stale.as_bool() || resolved != path {
            let _ = remember(&resolved);
        }
        resolved
    }
}
#[cfg(target_os = "macos")]
pub use bookmarks::{remember, restore};
#[cfg(not(target_os = "macos"))]
pub fn remember(_: &Path) -> Result<(), String> {
    Ok(())
}
#[cfg(not(target_os = "macos"))]
pub fn restore(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Pick a visible document folder before opening an editable window. Cancel
/// returns without creating a document or changing the previous location.
#[cfg(target_os = "macos")]
pub fn choose_document_folder(message: &str) -> Option<PathBuf> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSOpenPanel, NSModalResponseOK};
    use objc2_foundation::NSString;
    let panel = NSOpenPanel::openPanel(MainThreadMarker::new().expect("main thread"));
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(false);
    panel.setAllowsMultipleSelection(false);
    panel.setCanCreateDirectories(true);
    panel.setMessage(Some(&NSString::from_str(message)));
    panel.setPrompt(Some(&NSString::from_str("Use This Folder")));
    if panel.runModal() != NSModalResponseOK { return None; }
    panel.URL()?.path().map(|p| PathBuf::from(p.to_string()))
}

#[cfg(not(target_os = "macos"))]
pub fn choose_document_folder(_: &str) -> Option<PathBuf> {
    Some(home().join("Documents").join("gravitynote"))
}

pub fn location_error(error: &str) {
    #[cfg(target_os = "macos")]
    {
        use objc2::MainThreadMarker;
        use objc2_app_kit::NSAlert;
        use objc2_foundation::NSString;
        let alert = NSAlert::new(MainThreadMarker::new().expect("main thread"));
        alert.setMessageText(&NSString::from_str("Could not use that folder"));
        alert.setInformativeText(&NSString::from_str(error));
        alert.addButtonWithTitle(&NSString::from_str("Choose Another Folder"));
        alert.runModal();
    }
    eprintln!("{error}");
}

/// Copy old documents without removing originals or overwriting different files.
/// A failed/partial migration can be retried safely before committing the choice.
pub fn copy_preserving(source: &Path, target: &Path) -> std::io::Result<()> {
    if source == target || !source.exists() { return Ok(()); }
    if source.is_dir() {
        std::fs::create_dir_all(target)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() {
                return Err(std::io::Error::other("A linked file needs to be moved manually."));
            }
            copy_preserving(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if target.exists() {
        if std::fs::read(source)? != std::fs::read(target)? {
            return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists,
                format!("{} already contains a different file. Choose an empty folder; your original files are unchanged.", target.display())));
        }
    } else {
        // create_new prevents a file appearing during migration from being overwritten.
        use std::io::Write;
        let bytes = std::fs::read(source)?;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(target)?;
        if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = std::fs::remove_file(target);
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    #[test]
    fn migration_preserves_originals_and_refuses_conflicting_files() {
        let dir = std::env::temp_dir().join(format!("document-migration-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let source = dir.join("old"); let target = dir.join("chosen");
        std::fs::create_dir_all(source.join("images")).unwrap();
        std::fs::write(source.join("document"), "my work").unwrap();
        std::fs::write(source.join("images/picture"), "image bytes").unwrap();
        copy_preserving(&source, &target).unwrap();
        copy_preserving(&source, &target).unwrap(); // retry is idempotent
        assert_eq!(std::fs::read(target.join("document")).unwrap(), b"my work");
        assert!(source.join("document").exists());
        assert_eq!(std::fs::read(target.join("images/picture")).unwrap(), b"image bytes");
        std::fs::write(target.join("document"), "another document").unwrap();
        assert_eq!(copy_preserving(&source, &target).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(target.join("document")).unwrap(), b"another document");
        assert_eq!(std::fs::read(source.join("document")).unwrap(), b"my work");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// A container's Documents subfolder is still hidden from the user. Keep both
/// that location and Application Support out of document selection.
pub fn private_document_location(path: &Path) -> bool {
    let home = home();
    path.starts_with(home.join("Library")) ||
        (home.to_string_lossy().contains("/Library/Containers/") && path.starts_with(home))
}

pub fn check_document_folder(folder: &Path) -> std::io::Result<()> {
    if private_document_location(folder) {
        return Err(std::io::Error::other("Choose a visible folder, such as a folder in Documents."));
    }
    std::fs::create_dir_all(folder)?;
    let probe = folder.join(format!(".document-write-check-{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&probe)?;
    file.sync_all()?;
    drop(file);
    std::fs::remove_file(probe)
}
