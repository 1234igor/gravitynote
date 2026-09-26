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
