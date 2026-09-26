//! Native macOS integrations that GPUI does not provide: a menu-bar (status
//! item) with a dropdown menu, and a global `Control+A` hotkey.
//!
//! # What this module does *not* do
//!
//! It never shows, hides, focuses or creates windows. It only translates user
//! intent (a menu click, a hotkey press) into a [`PlatformEvent`] and pushes it
//! down an [`std::sync::mpsc`] channel. GPUI owns the windows; the app polls the
//! receiver and decides what to do.
//!
//! # Main-thread requirement
//!
//! [`install`] must be called on the main thread, from inside the running
//! application — for GPUI that means inside `Application::new().run(|cx| { .. })`.
//! `NSStatusBar`, `NSMenu` and `NSMenuItem` are all main-thread-only AppKit
//! types, and Carbon's application event target only exists once the process has
//! an event dispatcher. Calling [`install`] off the main thread returns `Err`
//! rather than tripping an AppKit assertion.
//!
//! The returned [`PlatformHandle`] must be kept alive for as long as you want the
//! status item and the hotkey to exist: dropping it removes the status item,
//! unregisters the hotkey and tears down the Carbon event handler.
//!
//! # Why Carbon instead of an `NSEvent` global monitor
//!
//! `NSEvent::addGlobalMonitorForEventsMatchingMask` observes key events across the
//! whole system, so macOS gates it behind the Accessibility (TCC) permission —
//! the user has to visit System Settings, and the app has to be re-launched after
//! the toggle. Carbon's `RegisterEventHotKey` instead asks the window server to
//! *reserve* one specific chord and deliver it to us; because it cannot observe
//! anything else, it requires no permission at all and works while the app is in
//! the background. The Carbon Event Manager hot-key API is deprecated on paper but
//! still fully supported and is what most menu-bar apps use.
//!
//! # When the hotkey is already taken
//!
//! Hotkeys are first-come-first-served system-wide. If another running app (or a
//! macOS system shortcut) already owns `Control+A`, `RegisterEventHotKey` fails
//! with a non-zero `OSStatus` — usually `-9878` (`eventHotKeyExistsErr`). That is
//! *not* treated as a fatal error: [`install`] still returns `Ok`, the status item
//! and its menu still work, and [`PlatformHandle::warning`] returns a
//! human-readable explanation the app can surface in its UI. The hotkey is not
//! retried; quitting the conflicting app and restarting GravityNote is the fix.

use std::sync::mpsc::Receiver;

use crate::settings::Hotkey;

/// User intent reported by the menu-bar item or the global hotkey.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformEvent {
    /// Global Ctrl+A pressed, or "Show / Hide" chosen from the menu.
    Toggle,
    /// Menu: bring the window to the front (do not hide).
    Show,
    /// Menu: create a new note.
    NewNote,
    /// Menu: write a backup right now.
    BackupNow,
    /// Menu: quit the app.
    Quit,
}

impl PlatformEvent {
    /// Stable `NSMenuItem` tag for this event. Tags start at 1 so that the
    /// default tag of 0 never decodes to a real event.
    fn tag(self) -> isize {
        match self {
            PlatformEvent::Toggle => 1,
            PlatformEvent::Show => 2,
            PlatformEvent::NewNote => 3,
            PlatformEvent::BackupNow => 4,
            PlatformEvent::Quit => 5,
        }
    }

    /// Inverse of [`PlatformEvent::tag`].
    fn from_tag(tag: isize) -> Option<Self> {
        match tag {
            1 => Some(PlatformEvent::Toggle),
            2 => Some(PlatformEvent::Show),
            3 => Some(PlatformEvent::NewNote),
            4 => Some(PlatformEvent::BackupNow),
            5 => Some(PlatformEvent::Quit),
            _ => None,
        }
    }

    /// Every variant, in menu order. Only the tests need to enumerate them.
    #[cfg(test)]
    const ALL: [PlatformEvent; 5] = [
        PlatformEvent::Toggle,
        PlatformEvent::Show,
        PlatformEvent::NewNote,
        PlatformEvent::BackupNow,
        PlatformEvent::Quit,
    ];
}

pub use imp::PlatformHandle;

/// Show or hide the window's close / minimise / zoom buttons.
///
/// They are the only chrome left on a window that is meant to read as a sheet
/// of paper, so they stay out of sight until the pointer goes looking for them.
/// The window still drags by its top strip, and closing is also "Hide
/// GravityNote" in the menu; minimise and zoom are reachable only here, which is
/// why the strip reveals them rather than the app dropping them.
///
/// Must be called on the main thread. Does nothing if the window is not there.
pub fn set_window_buttons_visible(visible: bool) {
    imp::set_window_buttons_visible(visible);
}

/// Show the standard macOS About panel (app icon, name, version). Main thread.
pub fn show_about_panel() {
    imp::show_about_panel();
}

/// Hide every *other* application — the App menu's "Hide Others". Main thread.
pub fn hide_others() {
    imp::hide_others();
}

/// Unhide all applications — the App menu's "Show All". Main thread.
pub fn show_all() {
    imp::show_all();
}

/// Install the menu-bar status item and the global show/hide hotkey, registered
/// to `shortcut`.
///
/// MUST be called on the main thread, from inside the running application
/// (for GPUI: inside `Application::new().run(|cx| { ... })`).
///
/// Returns the handle plus the receiver the caller polls for events. Change the
/// chord later with [`PlatformHandle::set_shortcut`].
pub fn install(
    shortcut: Option<Hotkey>,
) -> Result<(PlatformHandle, Receiver<PlatformEvent>), String> {
    imp::install(shortcut)
}

// ---------------------------------------------------------------------------
// macOS implementation
// ---------------------------------------------------------------------------

mod imp {
    use super::PlatformEvent;
    use crate::settings::Hotkey;

    use std::ffi::c_void;
    use std::panic::AssertUnwindSafe;
    use std::ptr;
    use std::sync::mpsc::{channel, Receiver, Sender};

    use objc2::rc::Retained;
    use objc2::runtime::Sel;
    use objc2::{
        define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadOnly,
    };
    use objc2_app_kit::{
        NSAnimationContext, NSApplication, NSButton, NSEventModifierFlags, NSImage, NSMenu,
        NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSWindowButton,
        NSWindowStyleMask,
    };

    /// How long the traffic lights take to arrive or leave.
    const BUTTON_FADE: f64 = 0.16;
    use objc2_foundation::{
        MainThreadMarker, NSData, NSObject, NSObjectProtocol, NSSize, NSString,
    };

    /// Monochrome vector counterpart to the full-colour Dock icon. AppKit uses
    /// its alpha as a template mask, so one embedded asset follows both light
    /// and dark menu-bar appearances without maintaining separate bitmaps.
    const STATUS_ICON_SVG: &[u8] = include_bytes!("../assets/icon/GravityNoteMenuBar.svg");

    // -- Carbon Event Manager FFI --------------------------------------------

    type OSStatus = i32;
    type OSType = u32;
    type EventTargetRef = *mut c_void;
    type EventHotKeyRef = *mut c_void;
    type EventHandlerRef = *mut c_void;
    type EventHandlerCallRef = *mut c_void;
    type EventRef = *mut c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct EventHotKeyID {
        signature: OSType,
        id: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct EventTypeSpec {
        event_class: u32,
        event_kind: u32,
    }

    type EventHandlerProc =
        unsafe extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn GetApplicationEventTarget() -> EventTargetRef;

        fn RegisterEventHotKey(
            in_hot_key_code: u32,
            in_hot_key_modifiers: u32,
            in_hot_key_id: EventHotKeyID,
            in_target: EventTargetRef,
            in_options: u32,
            out_ref: *mut EventHotKeyRef,
        ) -> OSStatus;

        fn UnregisterEventHotKey(in_hot_key: EventHotKeyRef) -> OSStatus;

        fn InstallEventHandler(
            in_target: EventTargetRef,
            in_handler: EventHandlerProc,
            in_num_types: usize,
            in_list: *const EventTypeSpec,
            in_user_data: *mut c_void,
            out_ref: *mut EventHandlerRef,
        ) -> OSStatus;

        fn RemoveEventHandler(in_handler_ref: EventHandlerRef) -> OSStatus;

        fn GetEventParameter(
            in_event: EventRef,
            in_name: u32,
            in_desired_type: u32,
            out_actual_type: *mut u32,
            in_buffer_size: usize,
            out_actual_size: *mut usize,
            out_data: *mut c_void,
        ) -> OSStatus;
    }

    /// `noErr`
    const NO_ERR: OSStatus = 0;
    /// `kEventClassKeyboard` — four-char code `'keyb'`.
    const K_EVENT_CLASS_KEYBOARD: u32 = 0x6B65_7962;
    /// `kEventHotKeyPressed`
    const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
    /// `kEventParamDirectObject` — four-char code `'----'`.
    const K_EVENT_PARAM_DIRECT_OBJECT: u32 = 0x2D2D_2D2D;
    /// `typeEventHotKeyID` — four-char code `'hkid'`.
    const TYPE_EVENT_HOT_KEY_ID: u32 = 0x686B_6964;
    /// `eventHotKeyExistsErr`
    const EVENT_HOT_KEY_EXISTS_ERR: OSStatus = -9878;

    /// Four-char code `'SNte'`, our private hot-key namespace.
    const HOT_KEY_SIGNATURE: OSType = 0x534E_7465;
    const HOT_KEY_ID: u32 = 1;

    /// Carbon hot-key callback.
    ///
    /// `user_data` is a `*mut Sender<PlatformEvent>` that outlives the handler
    /// (it is owned by [`PlatformHandle`] and freed only after the handler has
    /// been removed). The whole body is wrapped in `catch_unwind` because
    /// unwinding across the C frame would be undefined behaviour.
    unsafe extern "C" fn hot_key_handler(
        _call_ref: EventHandlerCallRef,
        event: EventRef,
        user_data: *mut c_void,
    ) -> OSStatus {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            if user_data.is_null() {
                return;
            }
            // SAFETY: `user_data` is the leaked `Sender` installed alongside this
            // handler; it is only freed after `RemoveEventHandler`.
            let tx = unsafe { &*(user_data as *const Sender<PlatformEvent>) };

            // Only react to *our* hot key. If the parameter cannot be read we
            // fall through and toggle anyway — we register exactly one hot key.
            let mut id = EventHotKeyID {
                signature: 0,
                id: 0,
            };
            // SAFETY: `event` is the live Carbon event; the out buffer matches
            // `typeEventHotKeyID`'s size.
            let status = unsafe {
                GetEventParameter(
                    event,
                    K_EVENT_PARAM_DIRECT_OBJECT,
                    TYPE_EVENT_HOT_KEY_ID,
                    ptr::null_mut(),
                    core::mem::size_of::<EventHotKeyID>(),
                    ptr::null_mut(),
                    (&mut id as *mut EventHotKeyID).cast::<c_void>(),
                )
            };
            if status == NO_ERR && (id.signature != HOT_KEY_SIGNATURE || id.id != HOT_KEY_ID) {
                return;
            }

            let _ = tx.send(PlatformEvent::Toggle);
        }));
        NO_ERR
    }

    // -- Menu action target ---------------------------------------------------

    struct MenuTargetIvars {
        tx: Sender<PlatformEvent>,
    }

    define_class!(
        // SAFETY:
        // - `NSObject` has no subclassing requirements.
        // - `MenuTarget` does not implement `Drop`; its ivars are dropped by the
        //   `dealloc` that `define_class!` generates.
        #[unsafe(super(NSObject))]
        #[name = "GravityNotePlatformMenuTarget"]
        #[ivars = MenuTargetIvars]
        struct MenuTarget;

        impl MenuTarget {
            /// Single action selector for every menu item; the item's `tag`
            /// selects the [`PlatformEvent`].
            #[unsafe(method(gravityNoteMenuAction:))]
            fn menu_action(&self, sender: *mut NSMenuItem) {
                if sender.is_null() {
                    return;
                }
                // SAFETY: AppKit passes the non-null `NSMenuItem` that was clicked.
                let item = unsafe { &*sender };
                if let Some(event) = PlatformEvent::from_tag(item.tag()) {
                    let _ = self.ivars().tx.send(event);
                }
            }
        }

        unsafe impl NSObjectProtocol for MenuTarget {}
    );

    impl MenuTarget {
        fn new(tx: Sender<PlatformEvent>) -> Retained<Self> {
            let this = Self::alloc().set_ivars(MenuTargetIvars { tx });
            unsafe { msg_send![super(this), init] }
        }

        fn action() -> Sel {
            sel!(gravityNoteMenuAction:)
        }
    }

    // -- Handle ---------------------------------------------------------------

    /// Owns the status item and the registered hotkey. Dropping it removes the
    /// status item and unregisters the hotkey.
    pub struct PlatformHandle {
        status_item: Retained<NSStatusItem>,
        /// Retained so the menu outlives the status item's unretained references.
        _menu: Retained<NSMenu>,
        /// The status-menu toggle item, kept so its ⌃A hint can be repainted when
        /// the shortcut changes.
        toggle_item: Retained<NSMenuItem>,
        /// `NSMenuItem::setTarget:` does not retain, so we must.
        _target: Retained<MenuTarget>,
        hot_key: EventHotKeyRef,
        handler: EventHandlerRef,
        /// Leaked `Sender` handed to the C callback; freed in `Drop`.
        sender: *mut Sender<PlatformEvent>,
        warning: Option<String>,
    }

    impl PlatformHandle {
        /// Human-readable note about degraded functionality (e.g. the hotkey could
        /// not be registered because another app owns the chord). `None` when all
        /// good.
        pub fn warning(&self) -> Option<&str> {
            self.warning.as_deref()
        }

        /// Re-register the global hotkey to a new chord and repaint the hint on
        /// the status menu. The Carbon *handler* stays installed — only the
        /// reserved chord changes — so this is cheap and keeps working across as
        /// many changes as the user makes.
        pub fn set_shortcut(&mut self, shortcut: Option<Hotkey>) {
            apply_toggle_hint(&self.toggle_item, shortcut.as_ref());

            // No handler means the event plumbing never came up (a rare install
            // failure); there is nothing to register the chord against.
            if self.handler.is_null() {
                return;
            }
            // SAFETY: `hot_key` is either null or a live registration from a
            // prior call; unregister it before replacing it.
            unsafe {
                if !self.hot_key.is_null() {
                    UnregisterEventHotKey(self.hot_key);
                    self.hot_key = ptr::null_mut();
                }
                // Turned off: the chord is unregistered and there is nothing to
                // warn about — the menu-bar icon still shows and hides.
                let Some(shortcut) = shortcut else {
                    self.warning = None;
                    return;
                };
                let Some((code, mods)) = shortcut.carbon() else {
                    self.warning = Some(format!(
                        "{} is not a key GravityNote can register globally. \
                         Pick another in Settings.",
                        shortcut.label()
                    ));
                    return;
                };
                let id = EventHotKeyID {
                    signature: HOT_KEY_SIGNATURE,
                    id: HOT_KEY_ID,
                };
                let mut hot_key: EventHotKeyRef = ptr::null_mut();
                // SAFETY: called on the main thread of a running app; the out
                // pointer is valid for the call.
                let status = RegisterEventHotKey(
                    code,
                    mods,
                    id,
                    GetApplicationEventTarget(),
                    0,
                    &mut hot_key,
                );
                if status == NO_ERR {
                    self.hot_key = hot_key;
                    self.warning = None;
                } else {
                    self.hot_key = ptr::null_mut();
                    self.warning = Some(if status == EVENT_HOT_KEY_EXISTS_ERR {
                        format!(
                            "Another application already owns {}, so the global \
                             shortcut is disabled. Pick a different one, or use the \
                             menu-bar icon.",
                            shortcut.label()
                        )
                    } else {
                        format!(
                            "Could not register the global {} hotkey (OSStatus {status}). \
                             Use the menu-bar icon instead.",
                            shortcut.label()
                        )
                    });
                }
            }
        }
    }

    impl Drop for PlatformHandle {
        fn drop(&mut self) {
            // SAFETY: each pointer is either null or the value produced by the
            // matching Carbon call in `install`, torn down exactly once.
            unsafe {
                if !self.handler.is_null() {
                    RemoveEventHandler(self.handler);
                    self.handler = ptr::null_mut();
                }
                if !self.hot_key.is_null() {
                    UnregisterEventHotKey(self.hot_key);
                    self.hot_key = ptr::null_mut();
                }
                if !self.sender.is_null() {
                    drop(Box::from_raw(self.sender));
                    self.sender = ptr::null_mut();
                }
            }
            NSStatusBar::systemStatusBar().removeStatusItem(&self.status_item);
        }
    }

    // -- window buttons -------------------------------------------------------

    pub fn set_window_buttons_visible(visible: bool) {
        let Some(mtm) = MainThreadMarker::new() else {
            // AppKit would assert; the buttons are cosmetic, so decline quietly.
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        // The process also owns the status item's window, and any open menu's,
        // so pick by what actually carries traffic lights rather than by
        // assuming there is only one window.
        for window in app.windows() {
            if !window.styleMask().contains(NSWindowStyleMask::Titled) {
                continue;
            }
            // Faded, not switched. Three saturated dots appearing instantly on
            // an otherwise silent page is the one moment this app raises its
            // voice — the same objection that got the caret its fade.
            let alpha = if visible { 1.0 } else { 0.0 };
            NSAnimationContext::beginGrouping();
            NSAnimationContext::currentContext().setDuration(BUTTON_FADE);
            for button in [
                NSWindowButton::CloseButton,
                NSWindowButton::MiniaturizeButton,
                NSWindowButton::ZoomButton,
            ] {
                if let Some(button) = window.standardWindowButton(button) {
                    // Alpha rather than `setHidden:`, which makes the window
                    // recompute its title bar and jump the traffic lights.
                    let animator: Retained<NSButton> = unsafe { msg_send![&*button, animator] };
                    animator.setAlphaValue(alpha);
                }
            }
            NSAnimationContext::endGrouping();
            return;
        }
    }

    /// The three NSApplication actions the menu wires up but GPUI does not wrap.
    /// Each is a no-op off the main thread (AppKit would assert).
    pub fn show_about_panel() {
        with_app(|app| {
            let nil: *mut objc2::runtime::AnyObject = ptr::null_mut();
            unsafe { let _: () = msg_send![app, orderFrontStandardAboutPanel: nil]; }
        });
    }

    pub fn hide_others() {
        with_app(|app| {
            let nil: *mut objc2::runtime::AnyObject = ptr::null_mut();
            unsafe { let _: () = msg_send![app, hideOtherApplications: nil]; }
        });
    }

    pub fn show_all() {
        with_app(|app| {
            let nil: *mut objc2::runtime::AnyObject = ptr::null_mut();
            unsafe { let _: () = msg_send![app, unhideAllApplications: nil]; }
        });
    }

    fn with_app(f: impl FnOnce(&NSApplication)) {
        if let Some(mtm) = MainThreadMarker::new() {
            f(&NSApplication::sharedApplication(mtm));
        }
    }

    // -- install --------------------------------------------------------------

    pub fn install(
        shortcut: Option<Hotkey>,
    ) -> Result<(PlatformHandle, Receiver<PlatformEvent>), String> {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| "platform::install() must be called on the main thread".to_string())?;

        let (tx, rx) = channel::<PlatformEvent>();

        let target = MenuTarget::new(tx.clone());
        let (menu, toggle_item) = build_menu(mtm, &target, shortcut.as_ref());

        let status_item =
            NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        if let Some(button) = status_item.button(mtm) {
            apply_status_icon(&button);
        }
        status_item.setMenu(Some(&menu));

        let (hot_key, handler, sender, warning) = install_hot_key(tx, shortcut.as_ref());

        Ok((
            PlatformHandle {
                status_item,
                _menu: menu,
                toggle_item,
                _target: target,
                hot_key,
                handler,
                sender,
                warning,
            },
            rx,
        ))
    }

    /// Set the display-only key-equivalent hint on the status menu's toggle item
    /// from a chord. Purely cosmetic: the real hotkey is the Carbon one. When the
    /// chord is off, or its key has no single-character equivalent (an arrow, a
    /// function key), the hint is cleared rather than shown wrong.
    fn apply_toggle_hint(item: &NSMenuItem, shortcut: Option<&Hotkey>) {
        let Some(shortcut) = shortcut else {
            item.setKeyEquivalent(&NSString::from_str(""));
            item.setKeyEquivalentModifierMask(NSEventModifierFlags::empty());
            return;
        };
        let key = match shortcut.key.as_str() {
            "space" => " ".to_string(),
            k if k.chars().count() == 1 => k.to_string(),
            _ => String::new(),
        };
        item.setKeyEquivalent(&NSString::from_str(&key));
        let mut mask = NSEventModifierFlags::empty();
        if shortcut.ctrl {
            mask |= NSEventModifierFlags::Control;
        }
        if shortcut.alt {
            mask |= NSEventModifierFlags::Option;
        }
        if shortcut.cmd {
            mask |= NSEventModifierFlags::Command;
        }
        if shortcut.shift {
            mask |= NSEventModifierFlags::Shift;
        }
        item.setKeyEquivalentModifierMask(mask);
    }

    fn build_menu(
        mtm: MainThreadMarker,
        target: &MenuTarget,
        shortcut: Option<&Hotkey>,
    ) -> (Retained<NSMenu>, Retained<NSMenuItem>) {
        let menu = NSMenu::new(mtm);
        // Our target implements the action, so AppKit would enable the items
        // anyway; being explicit keeps them live regardless of responder state.
        menu.setAutoenablesItems(false);

        // "Show / Hide GravityNote" carries the chord as a hint. A key equivalent
        // on a status-item menu is display-only — it is not in the main menu, so
        // AppKit never dispatches it as a real Cocoa shortcut; the actual chord
        // comes from Carbon below.
        let toggle = make_item(mtm, target, "Show / Hide GravityNote", PlatformEvent::Toggle);
        apply_toggle_hint(&toggle, shortcut);
        menu.addItem(&toggle);

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&make_item(mtm, target, "New Note", PlatformEvent::NewNote));
        menu.addItem(&make_item(mtm, target, "Back Up Now", PlatformEvent::BackupNow));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&make_item(mtm, target, "Quit GravityNote", PlatformEvent::Quit));

        (menu, toggle)
    }

    fn make_item(
        mtm: MainThreadMarker,
        target: &MenuTarget,
        title: &str,
        event: PlatformEvent,
    ) -> Retained<NSMenuItem> {
        // SAFETY: `MenuTarget` implements `gravityNoteMenuAction:`.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(MenuTarget::action()),
                &NSString::from_str(""),
            )
        };
        item.setTag(event.tag());
        item.setEnabled(true);
        // SAFETY: the target is unretained by AppKit but retained by
        // `PlatformHandle`, which outlives the menu.
        unsafe { item.setTarget(Some(target)) };
        item
    }

    /// Sets GravityNote's monochrome ringed-planet icon on the status-bar button.
    /// The SVG is decoded by AppKit and sized in points, so it stays sharp on
    /// every display.
    /// The old SF Symbol remains a safe fallback if the embedded asset ever fails
    /// to decode.
    fn apply_status_icon(button: &objc2_app_kit::NSStatusBarButton) {
        let image = MainThreadMarker::new()
            .and_then(|_| {
                // SAFETY: the embedded byte slice is valid for the duration of
                // the copy made by `NSData::dataWithBytes_length`.
                let data = unsafe {
                    NSData::dataWithBytes_length(
                        STATUS_ICON_SVG.as_ptr().cast(),
                        STATUS_ICON_SVG.len(),
                    )
                };
                let image = NSImage::initWithData(NSImage::alloc(), &data)?;
                image.setSize(NSSize::new(18.0, 18.0));
                Some(image)
            })
            .or_else(|| {
                NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str("note.text"),
                    Some(&NSString::from_str("GravityNote")),
                )
            });
        if let Some(image) = image {
            // A template image is recoloured by AppKit to match the menu bar in
            // both light and dark appearance.
            image.setTemplate(true);
            button.setImage(Some(&image));
        }
    }

    /// Registers the Carbon handler + hot key. Never fails the install: on error
    /// everything it allocated is released and a warning string is returned.
    fn install_hot_key(
        tx: Sender<PlatformEvent>,
        shortcut: Option<&Hotkey>,
    ) -> (
        EventHotKeyRef,
        EventHandlerRef,
        *mut Sender<PlatformEvent>,
        Option<String>,
    ) {
        let sender = Box::into_raw(Box::new(tx));
        let mut handler: EventHandlerRef = ptr::null_mut();
        let mut hot_key: EventHotKeyRef = ptr::null_mut();
        let mut warning: Option<String> = None;

        let spec = EventTypeSpec {
            event_class: K_EVENT_CLASS_KEYBOARD,
            event_kind: K_EVENT_HOT_KEY_PRESSED,
        };

        // SAFETY: called on the main thread of a running application; `spec` and
        // the out-pointers are valid for the duration of the call, and `sender`
        // stays alive until the handler is removed.
        let status = unsafe {
            InstallEventHandler(
                GetApplicationEventTarget(),
                hot_key_handler,
                1,
                &spec,
                sender.cast::<c_void>(),
                &mut handler,
            )
        };

        if status != NO_ERR {
            handler = ptr::null_mut();
            warning = Some(format!(
                "Could not install the keyboard event handler (OSStatus {status}); \
                 the global show/hide shortcut is disabled. Use the menu-bar icon instead."
            ));
        } else if let Some((code, mods)) = shortcut.and_then(Hotkey::carbon) {
            let id = EventHotKeyID {
                signature: HOT_KEY_SIGNATURE,
                id: HOT_KEY_ID,
            };
            // SAFETY: same as above.
            let status = unsafe {
                RegisterEventHotKey(
                    code,
                    mods,
                    id,
                    GetApplicationEventTarget(),
                    0,
                    &mut hot_key,
                )
            };
            if status != NO_ERR {
                hot_key = ptr::null_mut();
                let label = shortcut.map(Hotkey::label).unwrap_or_default();
                warning = Some(if status == EVENT_HOT_KEY_EXISTS_ERR {
                    format!(
                        "Another application already owns {label}, so the global hotkey is \
                         disabled. Pick a different one in Settings, or use the \
                         menu-bar icon."
                    )
                } else {
                    format!(
                        "Could not register the global {label} hotkey (OSStatus {status}). \
                         Use the menu-bar icon instead."
                    )
                });
            }
        }
        // When `shortcut` is `None` (turned off) the handler stays installed with
        // no chord registered, so a later `set_shortcut` can turn one back on.

        // The event handler must stay installed even with no chord registered, so
        // it is only torn down when the handler itself failed to install.
        if handler.is_null() {
            // SAFETY: `sender` came from `Box::into_raw`; it is not used again.
            unsafe {
                drop(Box::from_raw(sender));
            }
            return (ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), warning);
        }

        (hot_key, handler, sender, warning)
    }
}

// ---------------------------------------------------------------------------
// Non-macOS stub: identical public API, no-op behaviour.
// ---------------------------------------------------------------------------


#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn menu_tags_round_trip() {
        for event in PlatformEvent::ALL {
            assert_eq!(
                PlatformEvent::from_tag(event.tag()),
                Some(event),
                "tag round-trip failed for {event:?}"
            );
        }
    }

    #[test]
    fn menu_tags_are_unique_and_never_zero() {
        let tags: HashSet<isize> = PlatformEvent::ALL.iter().map(|e| e.tag()).collect();
        assert_eq!(tags.len(), PlatformEvent::ALL.len());
        assert!(!tags.contains(&0), "tag 0 is NSMenuItem's default");
    }

    #[test]
    fn unknown_tags_decode_to_none() {
        for tag in [-1, 0, 6, 99] {
            assert_eq!(PlatformEvent::from_tag(tag), None);
        }
    }

}
