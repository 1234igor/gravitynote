//! Open at login, through `SMAppService`.
//!
//! macOS 13 replaced the old login-item APIs with `SMAppService`, which
//! registers the *running app bundle* itself — no helper app, no launch agent
//! plist to install and keep in step. The system, not this app, remembers the
//! setting, so there is nothing here to persist: [`enabled`] asks it.
//!
//! Registration is a property of a bundle, so it only works when the app is
//! running as one. A bare `cargo run` binary has no bundle to register and says
//! so rather than failing quietly.

#[cfg(target_os = "macos")]
mod imp {
    use objc2_foundation::NSString;
    use objc2_service_management::{SMAppService, SMAppServiceStatus};

    fn main_app() -> objc2::rc::Retained<SMAppService> {
        // SAFETY: `mainAppService` is a class method with no arguments that
        // returns the shared service for this bundle. It is safe from any
        // thread.
        unsafe { SMAppService::mainAppService() }
    }

    pub fn enabled() -> bool {
        // SAFETY: reading the status of a valid service object.
        unsafe { main_app().status() == SMAppServiceStatus::Enabled }
    }

    pub fn set(on: bool) -> Result<(), String> {
        let service = main_app();
        // SAFETY: both calls take no arguments and report failure through the
        // returned `NSError` rather than by raising.
        let result = unsafe {
            if on {
                service.registerAndReturnError()
            } else {
                service.unregisterAndReturnError()
            }
        };
        result.map_err(|err| {
            let message: objc2::rc::Retained<NSString> = err.localizedDescription();
            message.to_string()
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn enabled() -> bool {
        false
    }

    pub fn set(_on: bool) -> Result<(), String> {
        Err("Opening at login is macOS-only".into())
    }
}

/// Whether the app is registered to open at login.
pub fn enabled() -> bool {
    imp::enabled()
}

/// Register or unregister the app as a login item.
///
/// The error is already phrased for a person: it is the system's own
/// explanation, which is more use than anything this module could invent.
pub fn set(on: bool) -> Result<(), String> {
    imp::set(on)
}
