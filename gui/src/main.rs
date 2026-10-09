//! corplink-gui: the menu-bar controller for the corplink-rs daemon.
//!
//! A hidden egui window hosts the login and log views; day-to-day control
//! happens through the native tray menu. The daemon owns the VPN: quitting
//! this app never touches the tunnel.

#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod ipc_client;
#[cfg(target_os = "macos")]
mod service;
#[cfg(target_os = "macos")]
mod setup;
#[cfg(target_os = "macos")]
mod state;
#[cfg(target_os = "macos")]
mod tray;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("corplink-gui only supports macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), eframe::Error> {
    use std::sync::mpsc;

    // the ipc thread starts immediately: the tray icon should reflect the
    // daemon state from the first moment
    let (event_tx, event_rx) = mpsc::channel();
    let outbound = ipc_client::spawn(event_tx);

    let (menu_tx, menu_rx) = mpsc::channel::<String>();
    tray::install_menu_handler(menu_tx);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_visible(false)
            .with_inner_size([440.0, 540.0])
            .with_min_inner_size([360.0, 240.0])
            .with_title("Corplink"),
        ..Default::default()
    };
    eframe::run_native(
        "corplink-gui",
        options,
        Box::new(move |cc| {
            Ok(Box::new(app::CorplinkApp::new(
                cc,
                event_rx,
                menu_rx,
                outbound,
            )))
        }),
    )
}
