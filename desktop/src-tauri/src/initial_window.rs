//! First-frame window reveal helpers.

#[cfg(target_os = "macos")]
pub(crate) const INITIAL_RENDER_READY_EVENT: &str = "initial-render-ready";

/// Plugin that reveals the main window once its restored geometry has settled
/// and React has committed the startup surface.
pub(crate) fn initial_window_reveal_plugin<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::<R, ()>::new("initial-window-reveal")
        .on_webview_ready(|webview| {
            if webview.label() != "main" {
                return;
            }

            // Linux/WebKitGTK needs media-stream settings and a
            // permission-request handler for getUserMedia; no-op
            // on macOS/Windows.
            crate::linux_media::enable_media_capture(&webview);

            // macOS applies the restored geometry asynchronously. Wait
            // for several identical outer bounds and for React to
            // commit the startup surface before revealing it.
            let window = webview.window();

            #[cfg(target_os = "macos")]
            {
                use tauri::{Listener, Manager};

                set_initial_window_backing(&window);

                let (initial_render_tx, initial_render_rx) = tokio::sync::oneshot::channel();
                window
                    .app_handle()
                    .once(INITIAL_RENDER_READY_EVENT, move |_| {
                        let _ = initial_render_tx.send(());
                    });

                tauri::async_runtime::spawn(async move {
                    wait_for_stable_initial_window_geometry(&window).await;

                    if tokio::time::timeout(std::time::Duration::from_secs(5), initial_render_rx)
                        .await
                        .is_err()
                    {
                        eprintln!(
                            "buzz-desktop: initial render did not commit before reveal timeout"
                        );
                    }

                    reveal_initial_window(&window);
                    clear_initial_window_backing(&window).await;
                });
            }

            #[cfg(not(target_os = "macos"))]
            {
                reveal_initial_window(&window);
            }
        })
        .build()
}

pub(crate) fn reveal_initial_window<R: tauri::Runtime>(window: &tauri::Window<R>) {
    if let Err(error) = window.show() {
        eprintln!("buzz-desktop: failed to reveal main window: {error}");
        return;
    }
    if let Err(error) = window.set_focus() {
        eprintln!("buzz-desktop: failed to focus main window: {error}");
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn set_initial_window_backing<R: tauri::Runtime>(window: &tauri::Window<R>) {
    // Both this write and the deferred clear target the Window (NSWindow)
    // backing color only; they never touch the webview canvas or the
    // NSVisualEffectView, so they are not load-bearing for glass. Glass state
    // — the effect view and webview-canvas transparency — is managed entirely
    // by `set_window_vibrancy`, which the ThemeProvider calls after mount. The
    // 250ms-delayed clear cannot clobber a persisted-glass-on cold boot
    // regardless of ordering with that call.
    //
    // Write an opaque dark backing so the previous app cannot show through
    // before WebKit submits its first composited surface.
    if let Err(error) = window.set_background_color(Some(tauri::window::Color(17, 21, 24, 255))) {
        eprintln!("buzz-desktop: failed to set initial window backing: {error}");
    }
}

#[cfg(target_os = "macos")]
pub(crate) async fn clear_initial_window_backing<R: tauri::Runtime>(window: &tauri::Window<R>) {
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    // Restore the default system window background so fast-resize gutter
    // flashes match the platform theme rather than the hardcoded dark color
    // written at reveal. Targets the Window (NSWindow) layer only; webview
    // canvas and glass state are unaffected.
    if let Err(error) = window.set_background_color(None) {
        eprintln!("buzz-desktop: failed to clear initial window backing: {error}");
    }
}

#[cfg(target_os = "macos")]
pub(crate) async fn wait_for_stable_initial_window_geometry<R: tauri::Runtime>(
    window: &tauri::Window<R>,
) {
    const MAX_POLLS: usize = 120;
    const REQUIRED_STABLE_POLLS: usize = 4;

    let mut previous_bounds = None;
    let mut stable_polls = 0;

    for _ in 0..MAX_POLLS {
        // Accept whatever geometry the window-state plugin restores — maximized
        // or a normal saved size. macOS applies the restore asynchronously, so
        // consecutive identical outer bounds are enough to know it settled.
        let bounds = match (window.outer_position(), window.outer_size()) {
            (Ok(position), Ok(size)) => Some((position.x, position.y, size.width, size.height)),
            _ => None,
        };

        if bounds.is_some() && bounds == previous_bounds {
            stable_polls += 1;
            if stable_polls >= REQUIRED_STABLE_POLLS {
                return;
            }
        } else {
            stable_polls = 0;
        }
        previous_bounds = bounds;

        tokio::time::sleep(std::time::Duration::from_millis(16)).await;
    }

    eprintln!("buzz-desktop: initial window geometry did not settle before reveal timeout");
}
