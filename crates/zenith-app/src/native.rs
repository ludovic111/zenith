//! macOS: what GPUI does not do for the window. The window is opaque: the sidebar's glass is
//! the web interface's material, drawn from an image (see `theme`), not the system's vibrancy.

// objc's `msg_send!` checks a `cargo-clippy` cfg that newer toolchains do not declare.
#![allow(unexpected_cfgs)]

use gpui::Window;

/// Development: `ZENITH_FLOAT=1` keeps the window above the others, without taking the focus, so
/// it keeps drawing while scripts look at it.
#[cfg(target_os = "macos")]
pub fn float_for_scripts(window: &Window) {
    use cocoa::base::{id, nil};
    use objc::{msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    if std::env::var_os("ZENITH_FLOAT").is_none() {
        return;
    }
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else { return };
    let view = appkit.ns_view.as_ptr() as id;
    // SAFETY: plain AppKit calls on the main thread, on the window this view belongs to.
    unsafe {
        let ns_window: id = msg_send![view, window];
        if ns_window == nil {
            return;
        }
        let _: () = msg_send![ns_window, setLevel: 3i64];
        // On every Space (canJoinAllSpaces | fullScreenAuxiliary).
        let _: () = msg_send![ns_window, setCollectionBehavior: 1u64 | 256u64];
        let _: () = msg_send![ns_window, orderFrontRegardless];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn float_for_scripts(_window: &Window) {}
