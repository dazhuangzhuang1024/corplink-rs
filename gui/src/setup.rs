//! The service window: the first-run setup that installs the service, and
//! later the account settings, reinstall and uninstall. The slow parts (the
//! company lookup, the administrator prompt and the install scripts) run on
//! worker threads and report back through a channel the UI polls.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

use corplink_ipc::ToDaemon;
use eframe::egui;
use serde_json::{Map, Value};

use crate::ipc_client::Outbound;
use crate::service::{self, AccountForm, Bundle, Platform, SplitDnsImport, SuggestedKind};
use crate::state::{next_id, GuiState, Link, Notice, Reconfig, ServiceStatus};

/// What a worker thread reports.
enum JobEvent {
    Progress(String),
    Done(JobDone),
}

enum JobDone {
    /// the first installation: config written and service installed
    Installed(Result<(), String>),
    Reinstalled(Result<(), String>),
    Uninstalled(Result<(), String>),
    /// account settings of an installed service, to be written once the
    /// daemon has disconnected
    SettingsReady(Result<(PathBuf, Map<String, Value>), String>),
}

pub struct ServicePanel {
    form: AccountForm,
    /// the config file the form was loaded from and saves to
    path: Option<PathBuf>,
    /// why the form could not be loaded; saving is off then
    load_error: Option<String>,
    progress: Option<String>,
    job: Option<Receiver<JobEvent>>,
    /// account settings waiting for the daemon to disconnect
    pending: Option<(PathBuf, Map<String, Value>)>,
    /// what the last import of the server's split DNS list did
    import_note: Option<String>,
    /// the server's split DNS domains the user unticked
    unpicked: HashSet<String>,
}

impl ServicePanel {
    pub fn new() -> ServicePanel {
        ServicePanel {
            form: AccountForm::default(),
            path: None,
            load_error: None,
            progress: None,
            job: None,
            pending: None,
            import_note: None,
            unpicked: HashSet::new(),
        }
    }

    /// Shows the window with the form loaded from the config file.
    pub fn open(&mut self, state: &mut GuiState) {
        state.show_service = true;
        state.touch();
        if !state.service_busy {
            self.load(state);
        }
    }

    fn load(&mut self, state: &GuiState) {
        self.form = AccountForm::default();
        self.load_error = None;
        self.import_note = None;
        self.unpicked.clear();
        self.path = state.service.config.clone();
        let Some(path) = &self.path else {
            self.load_error = Some(if state.service.installed {
                "读不出已安装服务使用的配置文件路径".to_string()
            } else {
                "找不到用户目录（HOME 没有设置）".to_string()
            });
            return;
        };
        match service::read_config(path) {
            Ok(Some(conf)) => self.form = AccountForm::from_config(&conf),
            Ok(None) => {}
            Err(e) => self.load_error = Some(e),
        }
    }

    /// Takes in what the workers and the daemon did since the last frame;
    /// true when a task finished, which may have changed the installation.
    pub fn poll(
        &mut self,
        state: &mut GuiState,
        bundle: Option<&Bundle>,
        outbound: &Sender<Outbound>,
    ) -> bool {
        let mut finished = false;
        while let Some(job) = &self.job {
            let event = match job.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // a worker never leaves without its result; be safe anyway
                    self.job = None;
                    self.progress = None;
                    state.finish_service_task(Notice::error("后台任务意外结束"));
                    finished = true;
                    break;
                }
            };
            match event {
                JobEvent::Progress(text) => self.progress = Some(text),
                JobEvent::Done(done) => {
                    self.job = None;
                    self.progress = None;
                    self.finish(done, state, bundle, outbound);
                    finished = true;
                }
            }
        }
        if state.reconfig == Reconfig::WriteDue {
            self.write_settings(state, outbound);
        }
        finished
    }

    fn finish(
        &mut self,
        done: JobDone,
        state: &mut GuiState,
        bundle: Option<&Bundle>,
        outbound: &Sender<Outbound>,
    ) {
        match done {
            JobDone::Installed(Ok(())) => {
                // the login prompts and the tray take it from here
                if let Some(bundle) = bundle {
                    match service::set_login_item(&bundle.app, true) {
                        Ok(()) => state.login_item = true,
                        Err(e) => eprintln!("[gui] failed to add the login item: {e}"),
                    }
                }
                state.finish_service_task(Notice::info("服务已安装，正在连接…"));
                state.show_service = false;
            }
            JobDone::Installed(Err(e)) => state.finish_service_task(Notice::error(e)),
            JobDone::Reinstalled(result) => state.finish_service_task(match result {
                Ok(()) => Notice::info("服务已用 App 内置的版本重新安装"),
                Err(e) => Notice::error(e),
            }),
            JobDone::Uninstalled(result) => state.finish_service_task(match result {
                Ok(()) => Notice::info("服务已卸载，配置文件仍然保留"),
                Err(e) => Notice::error(e),
            }),
            JobDone::SettingsReady(Err(e)) => state.finish_service_task(Notice::error(e)),
            JobDone::SettingsReady(Ok(pending)) => {
                self.pending = Some(pending);
                if state.link == Link::Connected {
                    self.progress = Some("正在断开 VPN…".to_string());
                    let id = next_id();
                    let _ = outbound.send(Outbound::Send(ToDaemon::Disconnect { id }));
                    state.reconfig = Reconfig::Disconnecting(id);
                } else {
                    state.reconfig = Reconfig::WriteDue;
                }
            }
        }
    }

    /// The session is down (or the daemon is not running): write the account
    /// settings, then have the daemon restart to read them.
    fn write_settings(&mut self, state: &mut GuiState, outbound: &Sender<Outbound>) {
        let Some((path, conf)) = self.pending.take() else {
            state.reconfig = Reconfig::Idle;
            return;
        };
        if let Err(e) = service::write_config(&path, &conf) {
            self.progress = None;
            state.finish_service_task(Notice::error(format!("无法写入 {}：{e}", path.display())));
            return;
        }
        if state.link == Link::Connected {
            self.progress = Some("正在重启服务…".to_string());
            let id = next_id();
            let _ = outbound.send(Outbound::Send(ToDaemon::Restart { id }));
            state.reconfig = Reconfig::Restarting(id);
        } else {
            self.progress = None;
            state.finish_service_task(Notice::info(
                "配置已保存。服务现在没有运行，launchd 下次启动它时会读到新配置",
            ));
        }
    }

    fn spawn(
        &mut self,
        state: &mut GuiState,
        work: impl FnOnce(&Sender<JobEvent>) -> JobDone + Send + 'static,
    ) {
        let (tx, rx) = channel();
        state.service_busy = true;
        state.service_notice = None;
        state.touch();
        self.job = Some(rx);
        self.progress = None;
        std::thread::Builder::new()
            .name("service-task".into())
            .spawn(move || {
                let done = work(&tx);
                let _ = tx.send(JobEvent::Done(done));
            })
            .expect("failed to spawn a service task");
    }

    /// The first installation: check the company code, write the config,
    /// then install behind the administrator prompt.
    fn start_install(&mut self, state: &mut GuiState, bundle: Bundle) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let form = self.form.clone();
        self.spawn(state, move |events| {
            let result = (|| {
                let conf = settings(&path, &form, events)?;
                service::write_config(&path, &conf)
                    .map_err(|e| format!("无法写入 {}：{e}", path.display()))?;
                progress(events, "请在系统弹出的窗口中输入管理员密码…");
                service::install_service(&bundle, &path).map_err(|e| e.to_string())
            })();
            JobDone::Installed(result)
        });
    }

    /// Account settings of the installed service; the write waits for the
    /// daemon, see [`Reconfig`].
    fn start_save(&mut self, state: &mut GuiState) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let form = self.form.clone();
        self.spawn(state, move |events| {
            JobDone::SettingsReady(settings(&path, &form, events).map(|conf| (path, conf)))
        });
    }

    pub fn start_reinstall(&mut self, state: &mut GuiState, bundle: Bundle) {
        self.open(state);
        self.spawn(state, move |events| {
            progress(events, "请在系统弹出的窗口中输入管理员密码…");
            JobDone::Reinstalled(service::reinstall_service(&bundle).map_err(|e| e.to_string()))
        });
    }

    pub fn start_uninstall(&mut self, state: &mut GuiState, bundle: Bundle) {
        self.open(state);
        self.spawn(state, move |events| {
            progress(events, "请在系统弹出的窗口中输入管理员密码…");
            JobDone::Uninstalled(service::uninstall_service(&bundle).map_err(|e| e.to_string()))
        });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, state: &mut GuiState, bundle: Option<&Bundle>) {
        let installed = state.service.installed;
        let busy = state.service_busy;
        ui.heading(if installed { "设置" } else { "首次设置" });
        ui.add_space(6.0);
        if !installed {
            ui.label("填写企业和账号信息，验证后会安装 VPN 服务（需要管理员密码）。");
            ui.add_space(4.0);
        }

        let imported = ui
            .add_enabled_ui(!busy && self.load_error.is_none(), |ui| {
                account_fields(ui, &mut self.form);
                server_split_dns(
                    ui,
                    &self.form.split_dns,
                    &state.server_split_dns,
                    &mut self.unpicked,
                )
            })
            .inner;
        if let Some(imported) = imported {
            self.form.split_dns = imported.text;
            let apply = if installed {
                "保存并重启服务"
            } else {
                "验证并安装"
            };
            self.import_note = Some(format!(
                "导入了 {} 个域名，点「{apply}」后生效",
                imported.added
            ));
        }
        if let Some(note) = &self.import_note {
            ui.label(note);
        }
        if let Some(e) = &self.load_error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.add_space(8.0);

        let problem = self.form.problem();
        let can_edit = !busy && self.load_error.is_none() && self.path.is_some();
        let placement = bundle.and_then(Bundle::placement_problem);
        ui.horizontal(|ui| {
            if !installed {
                let ready =
                    can_edit && problem.is_none() && bundle.is_some() && placement.is_none();
                if ui
                    .add_enabled(ready, egui::Button::new("验证并安装"))
                    .clicked()
                {
                    if let Some(bundle) = bundle {
                        self.start_install(state, bundle.clone());
                    }
                }
            } else if ui
                .add_enabled(
                    can_edit && problem.is_none(),
                    egui::Button::new("保存并重启服务"),
                )
                .clicked()
            {
                self.start_save(state);
            }
            if ui.button("关闭窗口").clicked() {
                state.show_service = false;
                state.touch();
            }
        });
        if can_edit {
            if let Some(problem) = problem {
                ui.label(problem);
            }
        }
        if !installed && bundle.is_none() {
            ui.label("开发模式：这个 GUI 没有打包服务，请用 scripts/install-launchd.sh 安装");
        }
        if let Some(placement) = placement {
            ui.colored_label(ui.visuals().warn_fg_color, placement);
        }

        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(self.progress.as_deref().unwrap_or("请稍候…"));
            });
        } else if let Some(notice) = &state.service_notice {
            if notice.error {
                ui.colored_label(ui.visuals().error_fg_color, &notice.text);
            } else {
                ui.label(&notice.text);
            }
        }

        ui.add_space(8.0);
        service_facts(ui, state);
    }
}

/// The form's fields; changes go straight into `form`.
fn account_fields(ui: &mut egui::Ui, form: &mut AccountForm) {
    egui::Grid::new("account-form")
        .num_columns(2)
        .spacing([10.0, 6.0])
        .show(ui, |ui| {
            ui.label("企业代码");
            ui.add(egui::TextEdit::singleline(&mut form.company).desired_width(240.0));
            ui.end_row();

            ui.label("用户名");
            ui.add(egui::TextEdit::singleline(&mut form.username).desired_width(240.0));
            ui.end_row();

            ui.label("密码");
            ui.add(
                egui::TextEdit::singleline(&mut form.password)
                    .password(true)
                    .hint_text("扫码或邮箱验证码登录可不填")
                    .desired_width(240.0),
            );
            ui.end_row();

            ui.label("登录方式");
            egui::ComboBox::from_id_salt("platform")
                .selected_text(form.platform.label())
                .width(240.0)
                .show_ui(ui, |ui| {
                    if let Platform::Other(_) = &form.platform {
                        let current = form.platform.clone();
                        let label = current.label();
                        ui.selectable_value(&mut form.platform, current, label);
                    }
                    for choice in Platform::CHOICES {
                        let label = choice.label();
                        ui.selectable_value(&mut form.platform, choice, label);
                    }
                });
            ui.end_row();

            ui.label("固定节点");
            ui.add(
                egui::TextEdit::singleline(&mut form.vpn_server)
                    .hint_text("可选，留空自动选择")
                    .desired_width(240.0),
            );
            ui.end_row();

            ui.label("分流域名");
            ui.add(
                egui::TextEdit::multiline(&mut form.split_dns)
                    .hint_text("可选，每行一个，例如 intranet.example.com")
                    .desired_rows(3)
                    .desired_width(240.0),
            );
            ui.end_row();
        });
    ui.label(
        "分流域名改用 VPN 分配的 DNS 解析，系统其他 DNS 设置不变；\
         CNAME 指向的域名也要列上。",
    );
}

/// The server's split DNS list entry by entry: what each one is, whether the
/// field has it already, and ticks to import the rest (all ticked unless
/// `unpicked`). What an import gives is returned for the caller to apply.
fn server_split_dns(
    ui: &mut egui::Ui,
    current: &str,
    suggested: &[String],
    unpicked: &mut HashSet<String>,
) -> Option<SplitDnsImport> {
    if suggested.is_empty() {
        ui.weak("连上 VPN 后，这里会逐条列出服务端下发的分流域名，可以勾选导入。");
        return None;
    }
    let rows = service::review_split_dns(current, suggested);
    let new: Vec<&str> = rows
        .iter()
        .filter_map(|row| match &row.kind {
            SuggestedKind::New { domain } => Some(domain.as_str()),
            _ => None,
        })
        .collect();
    let picked: Vec<String> = new
        .iter()
        .filter(|domain| !unpicked.contains(**domain))
        .map(|domain| domain.to_string())
        .collect();
    let mut imported = None;
    egui::CollapsingHeader::new(format!(
        "服务端下发的分流域名：{} 个，{} 个还没加入",
        rows.len(),
        new.len()
    ))
    .id_salt("server-split-dns")
    .default_open(true)
    .show(ui, |ui| {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!new.is_empty(), egui::Button::new("全选").small())
                .clicked()
            {
                for domain in &new {
                    unpicked.remove(*domain);
                }
            }
            if ui
                .add_enabled(!new.is_empty(), egui::Button::new("全不选").small())
                .clicked()
            {
                unpicked.extend(new.iter().map(|domain| domain.to_string()));
            }
            let import = egui::Button::new(format!("导入选中的 {} 个", picked.len()));
            if ui.add_enabled(!picked.is_empty(), import).clicked() {
                imported = Some(service::import_split_dns(current, &picked));
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("server-split-dns-list")
            .max_height(200.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for row in &rows {
                    ui.horizontal_wrapped(|ui| match &row.kind {
                        SuggestedKind::New { domain } => {
                            let mut picked = !unpicked.contains(domain);
                            let label = entry_label(&row.entry, domain);
                            if ui.checkbox(&mut picked, label).changed() {
                                if picked {
                                    unpicked.remove(domain);
                                } else {
                                    unpicked.insert(domain.clone());
                                }
                            }
                            // a resolver file for the parent answers for it
                            if let Some(parent) = new
                                .iter()
                                .find(|parent| domain.ends_with(&format!(".{parent}")))
                            {
                                ui.weak(format!("含在 {parent} 中"));
                            }
                        }
                        SuggestedKind::Listed { domain, by } => {
                            let label = entry_label(&row.entry, domain);
                            ui.add_enabled(false, egui::Checkbox::new(&mut true, label));
                            ui.weak(if by == domain {
                                "已在列表".to_string()
                            } else {
                                format!("已被 {by} 覆盖")
                            });
                        }
                        SuggestedKind::Invalid => {
                            ui.add_enabled(false, egui::Checkbox::new(&mut false, &row.entry));
                            ui.weak("不能用作分流域名");
                        }
                    });
                }
            });
    });
    imported
}

/// `*.example.com → example.com` where the entry is not the domain itself.
fn entry_label(entry: &str, domain: &str) -> String {
    if entry == domain {
        domain.to_string()
    } else {
        format!("{entry} → {domain}")
    }
}

/// Where the config is and which daemon runs.
fn service_facts(ui: &mut egui::Ui, state: &GuiState) {
    let ServiceStatus { config, .. } = &state.service;
    if let Some(config) = config {
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("配置文件：{}", config.display()));
            if ui.small_button("在 Finder 中显示").clicked() {
                service::reveal_in_finder(config);
            }
        });
        ui.label("路由、socks5 等其他高级选项请直接编辑配置文件。");
    }
    if state.link == Link::Connected {
        let line = format!(
            "服务版本 {} · App 版本 {}",
            state.status.version,
            env!("CARGO_PKG_VERSION")
        );
        if state.version_mismatch() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("{line}：建议「重新安装服务」"),
            );
        } else {
            ui.label(line);
        }
    }
}

/// The config to save: the file's content with the form over it, the
/// company code checked when it is new.
fn settings(
    path: &std::path::Path,
    form: &AccountForm,
    events: &Sender<JobEvent>,
) -> Result<Map<String, Value>, String> {
    let existing = service::read_config(path)?;
    let server = if service::needs_lookup(existing.as_ref(), form) {
        progress(events, "正在验证企业代码…");
        let company = service::lookup_company(&form.company)?;
        if !company.name.is_empty() {
            progress(events, &format!("企业：{}", company.name));
        }
        Some(company.server)
    } else {
        None
    };
    Ok(service::merge_config(existing, form, server.as_deref()))
}

fn progress(events: &Sender<JobEvent>, text: &str) {
    let _ = events.send(JobEvent::Progress(text.to_string()));
}
