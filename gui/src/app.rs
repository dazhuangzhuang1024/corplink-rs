//! The egui app: hidden by default, shown for the login and log windows.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use corplink_ipc::{LoginPrompt, ToDaemon};
use eframe::egui;
use eframe::egui::{ColorImage, TextureHandle, TextureOptions};

use crate::ipc_client::{AppEvent, Outbound};
use crate::service::{self, Bundle};
use crate::setup::ServicePanel;
use crate::state::{next_id, GuiState, Link, ServiceStatus};
use crate::tray;

/// How often the installation is looked at while no daemon answers, to
/// notice one installed or removed from the command line.
const SERVICE_CHECK_INTERVAL: Duration = Duration::from_secs(5);

pub struct CorplinkApp {
    state: GuiState,
    events: Receiver<AppEvent>,
    menu_events: Receiver<String>,
    outbound: std::sync::mpsc::Sender<Outbound>,
    tray: Option<tray::Tray>,
    built_revision: u64,
    // eframe shows the window once it painted the first frame, whatever the
    // viewport builder said, so it counts as visible until the first update
    // hides it
    viewport_visible: bool,
    // set by the quit item: the close it requests must not be cancelled
    quitting: bool,
    // the daemon and scripts this app carries; None for a bare `cargo run`
    bundle: Option<Bundle>,
    panel: ServicePanel,
    last_service_check: Instant,
    // the login QR, rebuilt whenever its url changes
    qr_url: Option<String>,
    qr_texture: Option<TextureHandle>,
}

impl CorplinkApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        events: Receiver<AppEvent>,
        menu_events: Receiver<String>,
        outbound: std::sync::mpsc::Sender<Outbound>,
    ) -> Self {
        install_cjk_fonts(&cc.egui_ctx);
        let bundle = Bundle::current();
        let mut state = GuiState::default();
        refresh_service(&mut state, bundle.as_ref());
        if let Some(bundle) = &bundle {
            // the app may have moved since the login item was made
            service::refresh_login_item(&bundle.app);
        }
        let mut panel = ServicePanel::new();
        if !state.service.installed {
            // first run: straight to the setup
            panel.open(&mut state);
        }
        let tray = tray::Tray::new(&state);
        CorplinkApp {
            built_revision: state.revision,
            state,
            events,
            menu_events,
            outbound,
            tray,
            viewport_visible: true,
            quitting: false,
            bundle,
            panel,
            last_service_check: Instant::now(),
            qr_url: None,
            qr_texture: None,
        }
    }

    fn send(&self, msg: ToDaemon) {
        let _ = self.outbound.send(Outbound::Send(msg));
    }

    fn pump_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                AppEvent::Connected => {
                    let follow = self.state.on_connected();
                    // the daemon greets with status and the standing prompt;
                    // logs only stream after an explicit request
                    self.send(ToDaemon::Logs {
                        id: next_id(),
                        tail: 400,
                    });
                    for msg in follow {
                        self.send(msg);
                    }
                    refresh_service(&mut self.state, self.bundle.as_ref());
                }
                AppEvent::Disconnected => {
                    self.state.on_disconnected();
                    refresh_service(&mut self.state, self.bundle.as_ref());
                }
                AppEvent::Frame(frame) => {
                    for msg in self.state.apply(&frame) {
                        self.send(msg);
                    }
                }
            }
        }
    }

    fn pump_menu(&mut self, ctx: &egui::Context) {
        while let Ok(id) = self.menu_events.try_recv() {
            use tray::ids;
            if let Some(server) = id.strip_prefix(ids::SERVER_PREFIX) {
                let msg = self.state.connect(Some(server.to_string()));
                self.send(msg);
                continue;
            }
            match id.as_str() {
                ids::TOGGLE => {
                    if self.state.wants_connect() {
                        let msg = self.state.connect(None);
                        self.send(msg);
                    } else {
                        self.send(ToDaemon::Disconnect { id: next_id() });
                    }
                }
                ids::RECONNECT => {
                    let msg = self.state.connect(None);
                    self.send(msg);
                }
                ids::SERVERS_REFRESH => {
                    let msg = self.state.request_servers();
                    self.send(msg);
                }
                ids::LOGIN => {
                    if let Some(msg) = self.state.open_login() {
                        self.send(msg);
                    }
                }
                ids::LOGS => self.state.toggle_logs(),
                ids::SERVICE_SETUP => self.panel.open(&mut self.state),
                ids::SERVICE_REINSTALL => {
                    if let Some(bundle) = self.bundle.clone() {
                        self.panel.start_reinstall(&mut self.state, bundle);
                    }
                }
                ids::SERVICE_UNINSTALL => {
                    if let Some(bundle) = self.bundle.clone() {
                        self.panel.start_uninstall(&mut self.state, bundle);
                    }
                }
                ids::SERVICE_LOGIN_ITEM => {
                    if let Some(bundle) = &self.bundle {
                        let enable = !self.state.login_item;
                        if let Err(e) = service::set_login_item(&bundle.app, enable) {
                            eprintln!("[gui] failed to change the login item: {e}");
                        }
                        self.state.login_item = service::login_item_enabled();
                        self.state.touch();
                    }
                }
                ids::SERVICE_REVEAL => {
                    if let Some(config) = &self.state.service.config {
                        service::reveal_in_finder(config);
                    }
                }
                ids::QUIT => {
                    self.quitting = true;
                    let _ = self.outbound.send(Outbound::Quit);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                _ => {}
            }
        }
    }

    fn sync_tray(&mut self) {
        let Some(tray) = &mut self.tray else {
            return;
        };
        if self.state.revision != self.built_revision {
            self.built_revision = self.state.revision;
            tray.rebuild(&self.state);
        } else {
            tray.refresh_status(&self.state);
        }
    }

    fn qr_texture(&mut self, ctx: &egui::Context, url: &str) -> Option<&TextureHandle> {
        if self.qr_url.as_deref() != Some(url) {
            self.qr_texture = build_qr_texture(ctx, url);
            self.qr_url = Some(url.to_string());
        }
        self.qr_texture.as_ref()
    }

    fn login_panel(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.heading("登录");
        ui.add_space(6.0);

        let connected = self.state.link == Link::Connected;
        let prompt = self.state.prompt.clone();
        match (prompt, connected) {
            (Some(LoginPrompt::Qr { url }), true) => {
                ui.label("扫码授权，或在浏览器中打开下面的链接：");
                ui.add_space(4.0);
                if let Some(texture) = self.qr_texture(ctx, &url) {
                    ui.image(egui::load::SizedTexture::new(
                        texture.id(),
                        texture.size_vec2() * 3.0,
                    ));
                }
                ui.add_space(4.0);
                ui.hyperlink(&url);
                ui.add_space(6.0);
                if ui.button("我已扫码，继续").clicked() {
                    self.send(ToDaemon::LoginReply {
                        id: next_id(),
                        value: String::new(),
                    });
                }
            }
            (Some(LoginPrompt::EmailCode), true) => {
                ui.label("输入邮箱收到的验证码：");
                if let Some(value) =
                    code_input_and_submit(ui, &mut self.state.code_input, "提交验证码")
                {
                    self.send(ToDaemon::LoginReply {
                        id: next_id(),
                        value,
                    });
                }
            }
            (Some(LoginPrompt::TwoFa), true) => {
                ui.label("输入 6 位 2FA 验证码：");
                if let Some(value) =
                    code_input_and_submit(ui, &mut self.state.code_input, "提交验证码")
                {
                    self.send(ToDaemon::LoginReply {
                        id: next_id(),
                        value,
                    });
                }
            }
            (None, true) => {
                ui.label("当前没有等待输入的登录请求。");
                // only offered while it cannot cost a working session
                if self.state.can_start_login() && ui.button("重新发起登录").clicked() {
                    let msg = self.state.login_start();
                    self.send(msg);
                }
            }
            (_, false) => {
                ui.label("守护进程未连接，无法登录。启动服务后重试。");
            }
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if self.state.prompt.is_some() && ui.button("取消登录").clicked() {
                self.send(ToDaemon::LoginCancel { id: next_id() });
            }
            if ui.button("关闭窗口").clicked() {
                self.state.show_login = false;
            }
        });
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("日志");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("复制全部").clicked() {
                let all = self
                    .state
                    .logs
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.ctx().output_mut(|out| out.copied_text = all);
            }
            if ui.button("清空").clicked() {
                self.state.logs.clear();
            }
            if self.state.link != Link::Connected {
                ui.label("（连接断开，以下为断开前的日志）");
            }
        });
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for line in &self.state.logs {
                    ui.monospace(line);
                }
            });
    }
}

impl eframe::App for CorplinkApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_events();
        self.pump_menu(ctx);
        if self
            .panel
            .poll(&mut self.state, self.bundle.as_ref(), &self.outbound)
            || (self.state.link == Link::Disconnected
                && self.last_service_check.elapsed() >= SERVICE_CHECK_INTERVAL)
        {
            self.last_service_check = Instant::now();
            refresh_service(&mut self.state, self.bundle.as_ref());
        }
        // the window's close button only hides it: the app lives in the menu
        // bar, and letting the root viewport close would quit it
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.state.hide_windows();
        }
        self.sync_tray();

        let any_window = self.state.show_login || self.state.show_logs || self.state.show_service;
        if any_window != self.viewport_visible {
            self.viewport_visible = any_window;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(any_window));
            if any_window {
                // showing a window does not activate an agent app (no Dock
                // icon), so it would open behind the frontmost app
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }

        if !any_window {
            // nothing to draw; wake up now and then to poll the event channels
            ctx.request_repaint_after(Duration::from_millis(500));
            return;
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.state.show_login {
                        self.login_panel(ctx, ui);
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(10.0);
                    }
                    if self.state.show_service {
                        self.panel.ui(ui, &mut self.state, self.bundle.as_ref());
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(10.0);
                    }
                    if self.state.show_logs {
                        self.log_panel(ui);
                    }
                });
        });
        // keep polling while windows are up
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

/// Looks at the installation again: the launchd job, its config file, and
/// whether the GUI starts at login.
fn refresh_service(state: &mut GuiState, bundle: Option<&Bundle>) {
    let installed = service::installed_service();
    let status = ServiceStatus {
        installed: installed.is_some(),
        config: match installed {
            Some(config) => config,
            None => service::default_config_path(),
        },
        bundled: bundle.is_some(),
    };
    let login_item = service::login_item_enabled();
    if status != state.service || login_item != state.login_item {
        state.service = status;
        state.login_item = login_item;
        state.touch();
    }
}

/// The input row; returns the submitted, trimmed value when it was.
fn code_input_and_submit(ui: &mut egui::Ui, input: &mut String, button: &str) -> Option<String> {
    let mut submitted = None;
    ui.horizontal(|ui| {
        let edit = egui::TextEdit::singleline(input)
            .hint_text("验证码")
            .desired_width(140.0);
        let response = ui.add(edit);
        let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.button(button).clicked() || enter) && !input.trim().is_empty() {
            submitted = Some(input.trim().to_string());
        }
    });
    if submitted.is_some() {
        input.clear();
    }
    submitted
}

fn build_qr_texture(ctx: &egui::Context, url: &str) -> Option<TextureHandle> {
    let code = match qrcode::QrCode::new(url.as_bytes()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("[gui] failed to render the QR code: {e}");
            return None;
        }
    };
    let quiet = 3; // modules of white border, so scanners lock on
    let modules = code.width();
    let size = modules + quiet * 2;
    let code_range = quiet..quiet + modules;
    let mut pixels = Vec::with_capacity(size * size);
    for y in 0..size {
        for x in 0..size {
            let in_code = code_range.contains(&x) && code_range.contains(&y);
            let dark = in_code && code[(x - quiet, y - quiet)] == qrcode::Color::Dark;
            let v = if dark { 20 } else { 245 };
            pixels.push(egui::Color32::from_rgb(v, v, v));
        }
    }
    let image = ColorImage {
        size: [size, size],
        pixels,
    };
    // nearest, not linear: the 3x upscale must keep the modules sharp
    Some(ctx.load_texture("login-qr", image, TextureOptions::NEAREST))
}

/// egui ships no CJK glyphs; load one from the system so the Chinese labels
/// are not tofu boxes. Falls back silently to the bundled fonts.
fn install_cjk_fonts(ctx: &egui::Context) {
    use egui::FontData;
    let candidates = [
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
    ];
    let Some(font) = candidates
        .iter()
        .find_map(|path| std::fs::read(path).ok().map(FontData::from_owned))
    else {
        eprintln!("[gui] no system CJK font found, Chinese text may not render");
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("system-cjk".to_owned(), font);
    // appended last: it only fills the glyphs the bundled fonts lack
    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .expect("the default font families are known")
        .push("system-cjk".to_owned());
    fonts
        .families
        .get_mut(&egui::FontFamily::Monospace)
        .expect("the default font families are known")
        .push("system-cjk".to_owned());
    ctx.set_fonts(fonts);
}
