//! UI layer
//!
//! System tray icon + WebView2 main window.
//! The chat UI is served by the local axum server; the WebView2
//! window simply points to http://127.0.0.1:{port}.

use anyhow::Result;

use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::TrayIconBuilder;
use wry::WebViewBuilder;

/// Create a simple gray square icon (32x32 RGBA).
fn create_default_icon() -> tray_icon::Icon {
    let size = 32u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for _y in 0..size {
        for _x in 0..size {
            rgba.extend_from_slice(&[160, 160, 160, 255]);
        }
    }
    tray_icon::Icon::from_rgba(rgba, size, size).expect("Failed to create tray icon")
}

/// Run the UI event loop (blocking — must be called on the main thread).
pub fn run(port: u16) -> Result<()> {
    let event_loop = EventLoop::new();

    // ── Tray icon ──────────────────────────────────────────────────
    let menu = Menu::new();
    let open_item = MenuItem::new("Open", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    let open_id = open_item.id().clone();
    let quit_id = quit_item.id().clone();
    menu.append(&open_item).ok();
    menu.append(&quit_item).ok();

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Chitty Workspace")
        .with_icon(create_default_icon())
        .build()
        .expect("Failed to create tray icon");

    // ── Main window ────────────────────────────────────────────────
    let window = WindowBuilder::new()
        .with_title("Chitty Workspace")
        .with_inner_size(LogicalSize::new(1200.0, 800.0))
        .with_min_inner_size(LogicalSize::new(600.0, 400.0))
        .build(&event_loop)
        .expect("Failed to create window");

    let url = format!("http://127.0.0.1:{}", port);
    let _webview = WebViewBuilder::new(&window)
        .with_url(&url)
        .build()
        .expect("Failed to create WebView");

    tracing::info!("UI started — WebView pointing to {}", url);

    // ── Event loop ─────────────────────────────────────────────────
    let menu_channel = MenuEvent::receiver();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        // Check tray menu events
        if let Ok(menu_event) = menu_channel.try_recv() {
            if menu_event.id() == &open_id {
                window.set_visible(true);
                window.set_focus();
            } else if menu_event.id() == &quit_id {
                *control_flow = ControlFlow::Exit;
            }
        }

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                // Hide to tray instead of exiting
                window.set_visible(false);
            }
            _ => {}
        }
    });
}
