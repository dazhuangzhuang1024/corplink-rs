//! The GUI's model of the daemon, kept pure (no egui, no sockets) so the
//! event handling, menu labels and tray color are unit-testable.

use std::collections::VecDeque;

use corplink_ipc::{FromDaemon, LoginPrompt, RunState, Status, ToDaemon, VpnServer};

/// How many log lines the log window keeps (the daemon's ring holds more).
const MAX_LOG_LINES: usize = 5000;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Link {
    Disconnected,
    Connected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconKind {
    /// no daemon on the socket
    Dead,
    Idle,
    Connecting,
    Online,
    /// the daemon waits for a login the user has to do
    LoginNeeded,
}

#[derive(Debug)]
pub struct GuiState {
    pub link: Link,
    pub status: Status,
    pub servers: Vec<VpnServer>,
    pub servers_loading: bool,
    pub logs: VecDeque<String>,
    /// set once per connection after subscribing; live lines arrive as events
    pub logs_subscribed: bool,
    pub prompt: Option<LoginPrompt>,
    pub show_logs: bool,
    pub show_login: bool,
    pub code_input: String,
    /// one login nudge per daemon connection: when the first status of a
    /// freshly connected daemon says login_needed, offer to start it right
    /// away instead of waiting for a menu click
    login_nudged: bool,
    /// bumped whenever anything the tray shows changed, so the app knows to
    /// rebuild the menu and swap the icon
    pub revision: u64,
}

impl Default for GuiState {
    fn default() -> Self {
        GuiState {
            link: Link::Disconnected,
            status: Status::default(),
            servers: Vec::new(),
            servers_loading: false,
            logs: VecDeque::new(),
            logs_subscribed: false,
            prompt: None,
            show_logs: false,
            show_login: false,
            code_input: String::new(),
            login_nudged: false,
            revision: 0,
        }
    }
}

impl GuiState {
    fn touch(&mut self) {
        self.revision += 1;
    }

    /// Follow-ups to send when the daemon connection (re)appeared.
    pub fn on_connected(&mut self) -> Vec<ToDaemon> {
        self.link = Link::Connected;
        self.servers_loading = false;
        self.logs_subscribed = false;
        self.login_nudged = false;
        self.touch();
        Vec::new()
    }

    pub fn on_disconnected(&mut self) {
        self.link = Link::Disconnected;
        self.servers_loading = false;
        self.logs_subscribed = false;
        self.prompt = None;
        self.touch();
    }

    /// Apply one frame from the daemon, returning follow-up requests.
    pub fn apply(&mut self, frame: &FromDaemon) -> Vec<ToDaemon> {
        match frame {
            FromDaemon::StatusChanged { status } => {
                // the daemon's first status after connecting is the real
                // picture; nudging on it (not on the stale pre-connect state)
                // avoids restarting a session that is already fine
                let mut out = Vec::new();
                if status.state == RunState::LoginNeeded
                    && !self.login_nudged
                    && self.prompt.is_none()
                {
                    self.login_nudged = true;
                    self.show_login = true;
                    out.push(ToDaemon::LoginStart { id: next_id() });
                }
                // a daemon that starts waiting for a login surfaces the
                // window once; closing it then sticks
                if status.state == RunState::LoginNeeded
                    && self.status.state != RunState::LoginNeeded
                {
                    self.show_login = true;
                }
                self.status = status.clone();
                self.touch();
                return out;
            }
            FromDaemon::LogLine { line } => {
                self.push_log(line.clone());
            }
            FromDaemon::LoginPromptUpdate { prompt } => {
                if prompt.is_some() {
                    self.show_login = true;
                }
                self.prompt = prompt.clone();
                if self.prompt.is_none() {
                    self.code_input.clear();
                }
                self.touch();
            }
            FromDaemon::Reply { id, ok, error, data } => {
                self.apply_reply(*id, *ok, error.clone(), data.clone());
            }
        }
        Vec::new()
    }

    fn apply_reply(
        &mut self,
        _id: u64,
        ok: bool,
        error: Option<String>,
        data: Option<corplink_ipc::RespData>,
    ) {
        if !ok {
            if let Some(error) = error {
                self.push_log(format!("[gui] {}", error));
            }
            return;
        }
        match data {
            Some(corplink_ipc::RespData::Status { status }) => {
                self.status = status;
                self.touch();
            }
            Some(corplink_ipc::RespData::Servers { servers }) => {
                self.servers = servers;
                self.servers_loading = false;
                self.touch();
            }
            Some(corplink_ipc::RespData::Logs { lines }) => {
                self.logs.clear();
                for line in lines {
                    self.push_log(line.clone());
                }
                // the daemon sends live lines only after a logs request; one
                // request per connection is enough
                self.logs_subscribed = true;
            }
            None => {}
        }
    }

    fn push_log(&mut self, line: String) {
        if self.logs.len() == MAX_LOG_LINES {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
    }

    pub fn request_servers(&mut self) -> ToDaemon {
        self.servers_loading = true;
        self.touch();
        ToDaemon::Servers { id: next_id() }
    }

    pub fn connect(&mut self, server: Option<String>) -> ToDaemon {
        ToDaemon::Connect { id: next_id(), server }
    }

    pub fn login_start(&mut self) -> ToDaemon {
        ToDaemon::LoginStart { id: next_id() }
    }

    /// Whether a login can be started from the GUI: only while the daemon
    /// has no session and none on the way. Elsewhere a `login_start` would
    /// tear a working session down.
    pub fn can_start_login(&self) -> bool {
        self.link == Link::Connected
            && self.prompt.is_none()
            && matches!(self.status.state, RunState::Idle | RunState::LoginNeeded)
    }

    /// The login menu item: shows the window, and starts the login the
    /// daemon is waiting for, if any.
    pub fn open_login(&mut self) -> Option<ToDaemon> {
        self.show_login = true;
        (self.status.state == RunState::LoginNeeded && self.can_start_login())
            .then(|| self.login_start())
    }

    pub fn toggle_logs(&mut self) {
        self.show_logs = !self.show_logs;
        // the menu labels the item after it
        self.touch();
    }

    pub fn hide_windows(&mut self) {
        self.show_login = false;
        self.show_logs = false;
        self.touch();
    }

    /// The tray color, straight from the model.
    pub fn icon_kind(&self) -> IconKind {
        match self.link {
            Link::Disconnected => IconKind::Dead,
            Link::Connected => match self.status.state {
                RunState::Online => IconKind::Online,
                RunState::Connecting => IconKind::Connecting,
                RunState::LoginNeeded => IconKind::LoginNeeded,
                RunState::Idle => IconKind::Idle,
            },
        }
    }

    /// One-line status for the menu header.
    pub fn status_line(&self) -> String {
        match self.link {
            Link::Disconnected => "守护进程未连接".to_string(),
            Link::Connected => match self.status.state {
                RunState::Idle => "未连接".to_string(),
                RunState::Connecting => "连接中…".to_string(),
                RunState::Online => {
                    let mut line = format!("已连接 · {}", self.status.server.clone().unwrap_or_default());
                    if let Some(since) = self.status.since_unix {
                        let up = chrono_like_elapsed(since);
                        if !up.is_empty() {
                            line.push_str(&format!(" · {}", up));
                        }
                    }
                    line
                }
                RunState::LoginNeeded => "需要登录".to_string(),
            },
        }
    }

    /// Whether the connect/disconnect item should show "connect".
    pub fn wants_connect(&self) -> bool {
        matches!(
            (self.link, self.status.state),
            (Link::Connected, RunState::Idle) | (Link::Connected, RunState::LoginNeeded)
        ) || self.link == Link::Disconnected
    }

    /// The daemon's word on the state, e.g. why it is retrying or idle, cut
    /// to fit a menu line.
    pub fn detail_line(&self) -> Option<String> {
        const MAX_CHARS: usize = 80;
        if self.link != Link::Connected {
            return None;
        }
        let detail = self.status.detail.as_deref()?.trim();
        if detail.is_empty() {
            return None;
        }
        if detail.chars().count() <= MAX_CHARS {
            return Some(detail.to_string());
        }
        let cut: String = detail.chars().take(MAX_CHARS - 1).collect();
        Some(format!("{cut}…"))
    }
}

/// "1h23m" style elapsed since a unix timestamp, empty when in the future or
/// unknown-time. chrono would be overkill: wall-clock minutes are enough.
fn chrono_like_elapsed(since_unix: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = now.saturating_sub(since_unix);
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h{}m", mins % 60);
    }
    format!("{}d{}h", hours / 24, hours % 24)
}

/// Per-process request ids for the ipc protocol.
pub fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn online(server: &str) -> Status {
        Status {
            state: RunState::Online,
            server: Some(server.to_string()),
            since_unix: Some(1000),
            ..Status::default()
        }
    }

    #[test]
    fn icon_and_status_line_follow_the_link_and_state() {
        let mut state = GuiState::default();
        assert_eq!(state.icon_kind(), IconKind::Dead);
        assert_eq!(state.status_line(), "守护进程未连接");

        state.on_connected();
        assert_eq!(state.icon_kind(), IconKind::Idle);

        state.status.state = RunState::Connecting;
        assert_eq!(state.icon_kind(), IconKind::Connecting);

        state.status = online("Node03");
        state.status.since_unix = {
            // "now-ish" so the elapsed branch runs without sleeps
            Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
                    - 90,
            )
        };
        assert_eq!(state.icon_kind(), IconKind::Online);
        assert!(state.status_line().starts_with("已连接 · Node03 · "));
        assert!(state.status_line().ends_with('m') || state.status_line().contains("1m"));

        state.status.state = RunState::LoginNeeded;
        assert_eq!(state.icon_kind(), IconKind::LoginNeeded);
        assert_eq!(state.status_line(), "需要登录");
    }

    #[test]
    fn a_login_needed_daemon_is_nudged_on_its_first_status() {
        let mut state = GuiState::default();
        state.on_connected();
        // the first status says login needed: offer to start the login
        let follow = state.apply(&FromDaemon::StatusChanged {
            status: Status {
                state: RunState::LoginNeeded,
                ..Status::default()
            },
        });
        assert_eq!(follow.len(), 1);
        assert!(matches!(follow[0], ToDaemon::LoginStart { .. }));
        assert!(state.show_login);

        // a repeated status does not nudge again
        assert!(state
            .apply(&FromDaemon::StatusChanged {
                status: Status {
                    state: RunState::LoginNeeded,
                    ..Status::default()
                },
            })
            .is_empty());

        // a healthy daemon never triggers it, even across reconnects
        let mut fresh = GuiState::default();
        fresh.on_connected();
        assert!(fresh
            .apply(&FromDaemon::StatusChanged {
                status: Status {
                    state: RunState::Online,
                    ..Status::default()
                },
            })
            .is_empty());
    }

    #[test]
    fn the_login_window_opens_once_per_wait_and_can_be_closed() {
        let login_needed = FromDaemon::StatusChanged {
            status: Status {
                state: RunState::LoginNeeded,
                ..Status::default()
            },
        };
        let mut state = GuiState::default();
        state.on_connected();
        state.apply(&login_needed);
        assert!(state.show_login);

        // closed by the user: repeated statuses do not force it back open
        state.hide_windows();
        assert!(state.apply(&login_needed).is_empty());
        assert!(!state.show_login);

        // a new wait for a login does
        state.apply(&FromDaemon::StatusChanged {
            status: Status {
                state: RunState::Connecting,
                ..Status::default()
            },
        });
        state.apply(&login_needed);
        assert!(state.show_login);
    }

    #[test]
    fn opening_the_login_window_never_restarts_a_session() {
        let mut state = GuiState::default();
        state.on_connected();
        for running in [RunState::Online, RunState::Connecting] {
            state.status.state = running;
            state.show_login = false;
            assert!(state.open_login().is_none(), "{running:?}");
            assert!(state.show_login);
            assert!(!state.can_start_login());
        }

        // the daemon waits for a login: the menu item starts it
        state.status.state = RunState::LoginNeeded;
        assert!(matches!(state.open_login(), Some(ToDaemon::LoginStart { .. })));
        // but not a second one over a standing prompt
        state.prompt = Some(LoginPrompt::EmailCode);
        assert!(state.open_login().is_none());
    }

    #[test]
    fn the_detail_line_fits_a_menu() {
        let mut state = GuiState::default();
        state.status.detail = Some("retry 1 in 10s: dns failure".into());
        // nothing from a daemon that is not there
        assert_eq!(state.detail_line(), None);

        state.on_connected();
        assert_eq!(
            state.detail_line().as_deref(),
            Some("retry 1 in 10s: dns failure")
        );
        state.status.detail = Some("错".repeat(200));
        let line = state.detail_line().unwrap();
        assert_eq!(line.chars().count(), 80);
        assert!(line.ends_with('…'));
        state.status.detail = Some("  ".into());
        assert_eq!(state.detail_line(), None);
    }

    #[test]
    fn prompts_open_the_window_and_answers_clear_it() {
        let mut state = GuiState::default();
        let url = "https://auth.example/xyz".to_string();
        state.apply(&FromDaemon::LoginPromptUpdate {
            prompt: Some(LoginPrompt::Qr { url: url.clone() }),
        });
        assert_eq!(
            state.prompt,
            Some(LoginPrompt::Qr {
                url: url.clone(),
            })
        );
        assert!(state.show_login);

        state.code_input = "123456".into();
        state.apply(&FromDaemon::LoginPromptUpdate { prompt: None });
        assert!(state.prompt.is_none());
        assert!(state.code_input.is_empty(), "stale input must not leak into the next prompt");
    }

    #[test]
    fn server_replies_replace_the_list_and_clear_loading() {
        let mut state = GuiState::default();
        let req = state.request_servers();
        assert!(state.servers_loading);
        let id = match req {
            ToDaemon::Servers { id } => id,
            other => panic!("{other:?}"),
        };

        state
            .apply(&FromDaemon::Reply {
                id,
                ok: true,
                error: None,
                data: Some(corplink_ipc::RespData::Servers {
                    servers: vec![VpnServer {
                        name: "节点".into(),
                        en_name: "Node03".into(),
                        protocol: "tcp".into(),
                        latency_ms: Some(31),
                    }],
                }),
            })
            .clear();
        assert_eq!(state.servers.len(), 1);
        assert!(!state.servers_loading);
    }

    #[test]
    fn failed_replies_land_in_the_log() {
        let mut state = GuiState::default();
        state
            .apply(&FromDaemon::Reply {
                id: 9,
                ok: false,
                error: Some("already connecting".into()),
                data: None,
            })
            .clear();
        assert_eq!(state.logs.len(), 1);
        assert!(state.logs[0].contains("already connecting"));
    }

    #[test]
    fn log_lines_are_capped() {
        let mut state = GuiState::default();
        for i in 0..(MAX_LOG_LINES + 100) {
            state.apply(&FromDaemon::LogLine {
                line: format!("line {i}"),
            });
        }
        assert_eq!(state.logs.len(), MAX_LOG_LINES);
        assert_eq!(state.logs.front().unwrap(), "line 100");
        assert_eq!(state.logs.back().unwrap(), &format!("line {}", MAX_LOG_LINES + 99));
    }

    #[test]
    fn elapsed_formats_sensibly() {
        assert!(chrono_like_elapsed(0).len() > 3); // decades since epoch
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(chrono_like_elapsed(now - 30), "30s");
        assert_eq!(chrono_like_elapsed(now - 125), "2m");
        assert_eq!(chrono_like_elapsed(now - 3661), "1h1m");
        assert_eq!(chrono_like_elapsed(now - 26 * 3600), "1d2h");
    }
}
