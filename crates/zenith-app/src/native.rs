//! macOS: the window's material. The lsuite design puts the chrome on the system's own
//! vibrancy (`NSVisualEffectView`, Sidebar material, following the window's active state);
//! GPUI 0.2.2's built-in blur uses a colorless material with its tint removed, which shows
//! as plain transparency on recent macOS, so zenith adds the material itself, under GPUI's
//! view. The window itself is transparent; solid surfaces (the work) are painted by GPUI.

// objc's `msg_send!` checks a `cargo-clippy` cfg that newer toolchains do not declare.
#![allow(unexpected_cfgs)]

use gpui::Window;

#[cfg(target_os = "macos")]
pub fn add_vibrancy(window: &Window) {
    use cocoa::base::{id, nil};
    use cocoa::foundation::NSRect;
    use objc::{class, msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else { return };
    let view = appkit.ns_view.as_ptr() as id;
    // NSVisualEffectMaterialSidebar, BlendingModeBehindWindow, StateFollowsWindowActiveState,
    // NSViewWidthSizable | NSViewHeightSizable, NSWindowBelow.
    const SIDEBAR: i64 = 7;
    const BEHIND_WINDOW: i64 = 0;
    const FOLLOWS_WINDOW: i64 = 0;
    const RESIZES: u64 = 2 | 16;
    const BELOW: i64 = -1;
    // SAFETY: plain AppKit calls on the main thread, on views this window owns.
    unsafe {
        let ns_window: id = msg_send![view, window];
        if ns_window == nil {
            return;
        }
        let content: id = msg_send![ns_window, contentView];
        let bounds: NSRect = msg_send![content, bounds];
        let effect: id = msg_send![class!(NSVisualEffectView), alloc];
        let effect: id = msg_send![effect, initWithFrame: bounds];
        let _: () = msg_send![effect, setMaterial: SIDEBAR];
        let _: () = msg_send![effect, setBlendingMode: BEHIND_WINDOW];
        let _: () = msg_send![effect, setState: FOLLOWS_WINDOW];
        let _: () = msg_send![effect, setAutoresizingMask: RESIZES];
        let _: () = msg_send![content, addSubview: effect positioned: BELOW relativeTo: nil];
        let _: () = msg_send![effect, release];
        // Development: `ZENITH_FLOAT=1` keeps the window above the others, without taking
        // the focus, so it keeps drawing while scripts look at it.
        if std::env::var_os("ZENITH_FLOAT").is_some() {
            let _: () = msg_send![ns_window, setLevel: 3i64];
            // On every Space (canJoinAllSpaces | fullScreenAuxiliary).
            let _: () = msg_send![ns_window, setCollectionBehavior: 1u64 | 256u64];
            let _: () = msg_send![ns_window, orderFrontRegardless];
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn add_vibrancy(_window: &Window) {}
