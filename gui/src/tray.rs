//! The native tray icon and its menu.

use crate::state::{GuiState, IconKind, Link};
use corplink_ipc::RunState;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// Tray menu item ids the event handler reacts to.
pub mod ids {
    pub const TOGGLE: &str = "toggle";
    pub const RECONNECT: &str = "reconnect";
    pub const SERVERS_REFRESH: &str = "servers-refresh";
    pub const SERVER_PREFIX: &str = "server:";
    pub const LOGIN: &str = "login";
    pub const LOGS: &str = "logs";
    pub const QUIT: &str = "quit";
}

/// Menu item size on macOS; 2x for retina.
const ICON_SIZE: u32 = 44;

/// A filled circle in the state color, drawn pixel by pixel: no asset files,
/// no image crates.
pub fn icon_rgba(kind: IconKind) -> (Vec<u8>, u32, u32) {
    let (r, g, b) = match kind {
        IconKind::Dead => (0x9A, 0x9A, 0x9E),
        IconKind::Idle => (0x8E, 0x8E, 0x93),
        IconKind::Connecting => (0xFF, 0x9F, 0x0A),
        IconKind::Online => (0x30, 0xD1, 0x58),
        IconKind::LoginNeeded => (0xFF, 0x45, 0x3A),
    };
    let size = ICON_SIZE as f32;
    let center = (size - 1.0) / 2.0;
    let outer = center - 1.0;
    let inner = center - 5.0;
    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            let alpha = if dist <= inner {
                0xFF
            } else if dist <= outer {
                // a darker ring edge
                let shade = 0.7;
                rgba.extend_from_slice(&[
                    (r as f32 * shade) as u8,
                    (g as f32 * shade) as u8,
                    (b as f32 * shade) as u8,
                    0xFF,
                ]);
                continue;
            } else {
                0x00
            };
            rgba.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    (rgba, ICON_SIZE, ICON_SIZE)
}

fn icon(kind: IconKind) -> Option<Icon> {
    let (rgba, w, h) = icon_rgba(kind);
    Icon::from_rgba(rgba, w, h).ok()
}

fn item(id: &str, text: &str, enabled: bool) -> MenuItem {
    MenuItem::with_id(MenuId::new(id), text, enabled, None)
}

/// The tray icon and the status line of its menu, which is kept current
/// between rebuilds: the uptime in it moves on while nothing else changes.
pub struct Tray {
    icon: TrayIcon,
    status_item: MenuItem,
    status_text: String,
}

impl Tray {
    /// Creates the tray icon; dropping the `Tray` removes it.
    pub fn new(state: &GuiState) -> Option<Tray> {
        let (menu, status_item, status_text) = build_menu(state);
        let built = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon(state.icon_kind())?)
            .with_tooltip("Corplink")
            .build();
        match built {
            Ok(icon) => Some(Tray {
                icon,
                status_item,
                status_text,
            }),
            Err(e) => {
                eprintln!("[gui] failed to create the tray icon: {e}");
                None
            }
        }
    }

    /// Pushes a rebuilt menu and icon after the model changed.
    pub fn rebuild(&mut self, state: &GuiState) {
        let (menu, status_item, status_text) = build_menu(state);
        self.icon.set_menu(Some(Box::new(menu)));
        self.status_item = status_item;
        self.status_text = status_text;
        if let Some(icon) = icon(state.icon_kind()) {
            let _ = self.icon.set_icon(Some(icon));
        }
    }

    pub fn refresh_status(&mut self, state: &GuiState) {
        let text = state.status_line();
        if text != self.status_text {
            self.status_item.set_text(&text);
            self.status_text = text;
        }
    }
}

/// Builds the whole menu from the model, returning its status line item
/// too. Called on every revision change; native menus are cheap to rebuild.
fn build_menu(state: &GuiState) -> (Menu, MenuItem, String) {
    let menu = Menu::new();
    let status_text = state.status_line();
    let status_item = item("status", &status_text, false);
    let _ = menu.append(&status_item);
    if let Some(detail) = state.detail_line() {
        let _ = menu.append(&item("detail", &detail, false));
    }
    let _ = menu.append(&PredefinedMenuItem::separator());

    // without a daemon there is nobody to send a request to
    let daemon_up = state.link == Link::Connected;
    let toggle_label = if !state.wants_connect() {
        "断开"
    } else if state.status.state == RunState::LoginNeeded {
        "登录并连接"
    } else {
        "连接"
    };
    let _ = menu.append(&item(ids::TOGGLE, toggle_label, daemon_up));
    if daemon_up && state.status.state == RunState::Online {
        let _ = menu.append(&item(ids::RECONNECT, "重连", true));
    }

    // server picker
    let servers = Submenu::with_id(MenuId::new("servers"), "切换节点", daemon_up);
    if state.servers.is_empty() {
        let _ = servers.append(&item(
            "servers-empty",
            if state.servers_loading {
                "测量延迟中…"
            } else {
                "（先点「刷新延迟」获取列表）"
            },
            false,
        ));
    } else {
        for server in &state.servers {
            let label = match server.latency_ms {
                Some(ms) => format!("{} · {}ms", server.en_name, ms),
                None => format!("{} · 不可达", server.en_name),
            };
            let is_current = daemon_up
                && state.status.state == RunState::Online
                && state.status.server.as_deref() == Some(server.en_name.as_str());
            let _ = servers.append(&CheckMenuItem::with_id(
                MenuId::new(format!("{}{}", ids::SERVER_PREFIX, server.en_name)),
                &label,
                true,
                is_current,
                None,
            ));
        }
    }
    let _ = servers.append(&PredefinedMenuItem::separator());
    let _ = servers.append(&item(
        ids::SERVERS_REFRESH,
        if state.servers_loading {
            "测量延迟中…"
        } else {
            "刷新延迟"
        },
        !state.servers_loading,
    ));
    let _ = menu.append(&servers);

    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&item(
        ids::LOGIN,
        if state.status.state == RunState::LoginNeeded {
            "重新登录…"
        } else {
            "登录窗口…"
        },
        true,
    ));
    let logs_label = if state.show_logs { "隐藏日志" } else { "日志…" };
    let _ = menu.append(&item(ids::LOGS, logs_label, true));
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&item(ids::QUIT, "退出 GUI（VPN 不受影响）", true));
    (menu, status_item, status_text)
}

/// Routes native menu events to the app via the stored sender.
/// Must be called once, before any menu can fire.
pub fn install_menu_handler(sender: std::sync::mpsc::Sender<String>) {
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        // the handler must never panic into the native event loop
        let _ = sender.send(event.id.0.clone());
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_valid_rgba_with_state_colors() {
        for kind in [
            IconKind::Dead,
            IconKind::Idle,
            IconKind::Connecting,
            IconKind::Online,
            IconKind::LoginNeeded,
        ] {
            let (rgba, w, h) = icon_rgba(kind);
            assert_eq!(rgba.len(), (w * h * 4) as usize);
            assert!(rgba.chunks(4).any(|px| px[3] > 0), "{kind:?} fully transparent");
        }
        // the states must be distinguishable
        let (online, _, _) = icon_rgba(IconKind::Online);
        let (dead, _, _) = icon_rgba(IconKind::Dead);
        assert_ne!(online, dead);
    }
}
