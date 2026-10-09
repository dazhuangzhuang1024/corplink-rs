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
    /// the setup when nothing is installed, else the account settings
    pub const SERVICE_SETUP: &str = "service-setup";
    pub const SERVICE_REINSTALL: &str = "service-reinstall";
    pub const SERVICE_UNINSTALL: &str = "service-uninstall";
    pub const SERVICE_LOGIN_ITEM: &str = "service-login-item";
    pub const SERVICE_REVEAL: &str = "service-reveal";
}

/// Menu item size on macOS; 2x for retina.
const ICON_SIZE: u32 = 44;

/// The tray icon: a white padlock silhouette — the language the menu bar
/// already speaks — whose body interior fills with the state color: orange
/// while connecting, green once the tunnel is up, red when a login is
/// needed, hollow when there is nothing to report. A soft dark edge keeps
/// it readable on light and dark bars alike. Drawn pixel by pixel with 3x3
/// supersampling; no asset files, no image crates.
pub fn icon_rgba(kind: IconKind) -> (Vec<u8>, u32, u32) {
    let color = match kind {
        IconKind::Dead | IconKind::Idle => None,
        IconKind::Connecting => Some((0xFF as f32, 0x9F as f32, 0x0A as f32)),
        IconKind::Online => Some((0x32 as f32, 0xC7 as f32, 0x5E as f32)),
        IconKind::LoginNeeded => Some((0xFF as f32, 0x45 as f32, 0x3A as f32)),
    };
    let size = ICON_SIZE as f32;
    let center = (size - 1.0) / 2.0;
    let subs = 3;
    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            // premultiplied accumulation over the subsamples
            let (mut cr, mut cg, mut cb, mut ca) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..subs {
                for sx in 0..subs {
                    let px = x as f32 + (sx as f32 + 0.5) / subs as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / subs as f32;
                    let (r, g, b, a) = lock_shade(px - center, py - center, color);
                    cr += r * a;
                    cg += g * a;
                    cb += b * a;
                    ca += a;
                }
            }
            let samples = (subs * subs) as f32;
            if ca <= 0.0 {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                let a = ca / samples;
                rgba.extend_from_slice(&[
                    (cr / ca) as u8,
                    (cg / ca) as u8,
                    (cb / ca) as u8,
                    (a * 255.0).round() as u8,
                ]);
            }
        }
    }
    (rgba, ICON_SIZE, ICON_SIZE)
}

/// The color of one subsample of the padlock: a white stroke with a soft
/// dark edge behind it, and the body's interior in the state color (`None`
/// leaves it hollow). `x`/`y` relative to the icon center.
fn lock_shade(x: f32, y: f32, color: Option<(f32, f32, f32)>) -> (f32, f32, f32, f32) {
    // the body interior, inside the stroke
    if rounded_rect_sdf(x, y, 0.0, 6.7, 15.0, 11.2, 4.6) <= -1.5 {
        return match color {
            Some((r, g, b)) => (r, g, b, 0.97),
            None => (0.0, 0.0, 0.0, 0.0),
        };
    }
    let dist = lock_stroke_distance(x, y);
    if dist <= 1.5 {
        (255.0, 255.0, 255.0, 0.94)
    } else if dist <= 2.8 {
        // the edge fades out, keeping the glyph readable on any bar
        let a = 0.5 * (1.0 - (dist - 1.5) / 1.3);
        (11.0, 11.0, 12.0, a)
    } else {
        (0.0, 0.0, 0.0, 0.0)
    }
}

/// How far the point is from the padlock's stroke path: the rounded body's
/// outline and an arc shackle arching over it, its ends tucked into the
/// body's outline (a classic padlock silhouette).
fn lock_stroke_distance(x: f32, y: f32) -> f32 {
    let body = (rounded_rect_sdf(x, y, 0.0, 6.7, 15.0, 11.2, 4.6)).abs();
    let mut best = body;
    if y <= -2.0 {
        // the shackle rises out of the body
        let arc = ((x * x + (y + 3.8) * (y + 3.8)).sqrt() - 7.0).abs();
        best = best.min(arc);
    }
    best
}

/// The signed distance to an axis-aligned rounded rectangle centered at
/// (cx, cy) with half-extents (hx, hy) and corner radius r; negative inside.
fn rounded_rect_sdf(x: f32, y: f32, cx: f32, cy: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let qx = (x - cx).abs() - (hx - r);
    let qy = (y - cy).abs() - (hy - r);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
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
    if !daemon_up && !state.service.installed {
        // nothing else works before the service is there
        let _ = menu.append(&item(ids::SERVICE_SETUP, "安装服务…", !state.service_busy));
        append_tail(&menu, state);
        return (menu, status_item, status_text);
    }
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
            // some companies leave the English name empty
            let name = server.display_name();
            let label = match server.latency_ms {
                Some(ms) => format!("{} · {}ms", name, ms),
                None => format!("{} · 不可达", name),
            };
            let is_current = daemon_up
                && state.status.state == RunState::Online
                && state.status.server.as_deref() == Some(name);
            let _ = servers.append(&CheckMenuItem::with_id(
                MenuId::new(format!("{}{}", ids::SERVER_PREFIX, name)),
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
    if state.service.installed {
        let _ = menu.append(&service_menu(state));
    }
    append_tail(&menu, state);
    (menu, status_item, status_text)
}

/// The installed service: account, reinstall, uninstall, login item.
fn service_menu(state: &GuiState) -> Submenu {
    let service = &state.service;
    // the install scripts come with a packaged app only
    let manage = service.bundled && !state.service_busy;
    let menu = Submenu::with_id(MenuId::new("service"), "服务", true);
    let _ = menu.append(&item(ids::SERVICE_SETUP, "设置…", !state.service_busy));
    let reinstall = if state.version_mismatch() {
        "更新服务到 App 内置版本…"
    } else {
        "重新安装服务…"
    };
    let _ = menu.append(&item(ids::SERVICE_REINSTALL, reinstall, manage));
    let _ = menu.append(&item(ids::SERVICE_UNINSTALL, "卸载服务…", manage));
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&CheckMenuItem::with_id(
        MenuId::new(ids::SERVICE_LOGIN_ITEM),
        "登录时启动 GUI",
        service.bundled,
        state.login_item,
        None,
    ));
    let _ = menu.append(&item(
        ids::SERVICE_REVEAL,
        "在 Finder 中显示配置文件",
        service.config.is_some(),
    ));
    if state.link == Link::Connected {
        let _ = menu.append(&PredefinedMenuItem::separator());
        let versions = format!(
            "服务 {} · App {}",
            state.status.version,
            env!("CARGO_PKG_VERSION")
        );
        let _ = menu.append(&item("service-version", &versions, false));
    }
    menu
}

/// The items every menu ends with.
fn append_tail(menu: &Menu, state: &GuiState) {
    let logs_label = if state.show_logs { "隐藏日志" } else { "日志…" };
    let _ = menu.append(&item(ids::LOGS, logs_label, true));
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&item(ids::QUIT, "退出 GUI（VPN 不受影响）", true));
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
