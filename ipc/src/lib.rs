//! The wire protocol between the corplink-rs daemon and the corplink-gui
//! menu-bar app.
//!
//! Transport is a Unix domain socket carrying newline-delimited JSON: every
//! line is one [`ToDaemon`] message (GUI to daemon) or one [`FromDaemon`]
//! message (daemon to GUI). Requests carry a client-chosen `id`; the daemon's
//! [`FromDaemon::Reply`] echoes it so responses can be correlated with
//! requests. Everything else the daemon sends is an event pushed whenever the
//! world changes.
//!
//! This crate is deliberately dependency-light (serde only): the daemon links
//! the Go wireguard library and the GUI must not, so both sides share the
//! protocol through this crate instead of through the daemon crate.

use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- transport

/// Serializes `value` as one wire line, newline included.
pub fn encode<T: Serialize>(value: &T) -> String {
    let mut line = serde_json::to_string(value).expect("protocol messages serialize");
    line.push('\n');
    line
}

/// Parses one wire line (the newline may already be stripped).
pub fn decode<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

const SOCK_NAME: &str = "corplink-rs.sock";

/// Where the socket lives, in bind/connect attempt order: `$CORPLINK_IPC_SOCK`
/// when set, else `/var/run` (the root daemon) and the user's temp directory
/// (foreground runs without root, e.g. socks5/netstack mode). The daemon binds
/// the first candidate that works; the GUI connects to the first one that
/// answers, so both ends meet without knowing each other's privileges.
pub fn socket_candidates() -> Vec<PathBuf> {
    match env::var_os("CORPLINK_IPC_SOCK") {
        Some(path) => vec![PathBuf::from(path)],
        None => vec![Path::new("/var/run").join(SOCK_NAME), env::temp_dir().join(SOCK_NAME)],
    }
}

// ------------------------------------------------------------- GUI -> daemon

/// A request from the GUI. `id` is echoed back in the daemon's reply.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ToDaemon {
    /// Full status snapshot; also answered implicitly by later
    /// [`FromDaemon::StatusChanged`] events.
    Status { id: u64 },

    /// The VPN server list with freshly measured latencies. Takes seconds to
    /// answer; the reply arrives whenever the probes finish.
    Servers { id: u64 },

    /// Start (or restart) a session, optionally pinning the VPN server for
    /// this connect. Acknowledged, not completed, by the reply: watch
    /// [`FromDaemon::StatusChanged`] instead. `server: None` keeps the
    /// configured selection (vpn_server_name / env / strategy).
    Connect {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server: Option<String>,
    },

    /// Tear the current session down and go idle. The daemon process stays
    /// alive (launchd's KeepAlive would not restart a clean exit).
    Disconnect { id: u64 },

    /// Same as [`ToDaemon::Connect`], kept as its own verb for the GUI's
    /// "re-login" flow when the daemon sits in `login_needed`.
    LoginStart { id: u64 },

    /// Answer the current [`LoginPrompt`]: "scanned" for the QR prompt (any
    /// value), the code itself for the email/2fa prompts.
    LoginReply { id: u64, value: String },

    /// Abort the current login prompt; the login flow fails with a
    /// "cancelled" error.
    LoginCancel { id: u64 },

    /// The last `tail` buffered log lines; live lines then arrive as
    /// [`FromDaemon::LogLine`] events.
    Logs { id: u64, tail: u32 },
}

// ------------------------------------------------------------- daemon -> GUI

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FromDaemon {
    /// The outcome of a request. `data` is only set on success and only for
    /// requests that carry one (status/servers/logs).
    Reply {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<RespData>,
    },
    /// Pushed on every daemon state change.
    StatusChanged { status: Status },
    /// One live log line (after a `logs` request subscribed this connection).
    LogLine { line: String },
    /// A login prompt appeared (`Some`) or was answered/cancelled/expired
    /// (`None`). The GUI shows/hides its login window accordingly.
    LoginPromptUpdate { prompt: Option<LoginPrompt> },
}

/// Successful-reply payloads.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum RespData {
    Status { status: Status },
    Servers { servers: Vec<VpnServer> },
    Logs { lines: Vec<String> },
}

// -------------------------------------------------------------------- status

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// No session; waiting for a `connect`.
    #[default]
    Idle,
    /// Login and/or connection setup in flight; `detail` says where it is.
    Connecting,
    /// The tunnel is up.
    Online,
    /// The session needs credentials that only an interactive UI can provide.
    /// The daemon waits here instead of exiting (or thrashing under launchd)
    /// until the GUI starts a login.
    LoginNeeded,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct Status {
    pub state: RunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// Unix seconds of when the session came up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<String>,
    /// "tcp" or "udp" of the current session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub split_dns_domains: Vec<String>,
    /// Daemon crate version, for the GUI to notice mismatches.
    #[serde(default)]
    pub version: String,
    /// Progress ("logging in with lark…") or the last failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One row of the server list, with this probe's latency.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct VpnServer {
    pub name: String,
    pub en_name: String,
    /// "tcp" or "udp", as the server advertises per row.
    pub protocol: String,
    /// `None` when the probe failed (unreachable or needs login).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<i64>,
}

// -------------------------------------------------------------- login prompts

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoginPrompt {
    /// Scan the QR code for `url`; the reply's `value` is ignored and only
    /// means "I have scanned it, check the token".
    Qr { url: String },
    /// Enter the code that was just emailed.
    EmailCode,
    /// Enter the 6-digit 2FA code (config has no `code` secret).
    TwoFa,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_daemon_roundtrips() {
        let msgs = vec![
            ToDaemon::Status { id: 1 },
            ToDaemon::Servers { id: 2 },
            ToDaemon::Connect { id: 3, server: None },
            ToDaemon::Connect { id: 4, server: Some("Node03".into()) },
            ToDaemon::Disconnect { id: 5 },
            ToDaemon::LoginStart { id: 6 },
            ToDaemon::LoginReply { id: 7, value: "123456".into() },
            ToDaemon::LoginCancel { id: 8 },
            ToDaemon::Logs { id: 9, tail: 200 },
        ];
        for msg in &msgs {
            let line = encode(msg);
            assert!(line.ends_with('\n'));
            assert_eq!(&decode::<ToDaemon>(&line).unwrap(), msg);
        }
        assert!(decode::<ToDaemon>(&encode(&ToDaemon::Connect { id: 3, server: None }))
            .unwrap()
            .eq(&ToDaemon::Connect { id: 3, server: None }));
        // the wire form of a server-less connect carries no server field
        assert!(!encode(&ToDaemon::Connect { id: 3, server: None }).contains("server"));
    }

    #[test]
    fn from_daemon_roundtrips() {
        let status = Status {
            state: RunState::Online,
            server: Some("Node03".into()),
            since_unix: Some(1759900000),
            dns: Some("10.2.2.17".into()),
            protocol: Some("tcp".into()),
            split_dns_domains: vec!["corp.example".into()],
            version: "0.6.0".into(),
            detail: None,
        };
        let msgs = vec![
            FromDaemon::Reply {
                id: 1,
                ok: true,
                error: None,
                data: Some(RespData::Status { status: status.clone() }),
            },
            FromDaemon::Reply {
                id: 2,
                ok: false,
                error: Some("already connecting".into()),
                data: None,
            },
            FromDaemon::Reply {
                id: 3,
                ok: true,
                error: None,
                data: Some(RespData::Servers {
                    servers: vec![VpnServer {
                        name: "节点三".into(),
                        en_name: "Node03".into(),
                        protocol: "tcp".into(),
                        latency_ms: None,
                    }],
                }),
            },
            FromDaemon::Reply {
                id: 4,
                ok: true,
                error: None,
                data: Some(RespData::Logs { lines: vec!["one".into(), "two".into()] }),
            },
            FromDaemon::StatusChanged { status: status.clone() },
            FromDaemon::LogLine { line: "[info] hi".into() },
            FromDaemon::LoginPromptUpdate { prompt: Some(LoginPrompt::Qr { url: "https://x".into() }) },
            FromDaemon::LoginPromptUpdate { prompt: Some(LoginPrompt::EmailCode) },
            FromDaemon::LoginPromptUpdate { prompt: Some(LoginPrompt::TwoFa) },
            FromDaemon::LoginPromptUpdate { prompt: None },
        ];
        for msg in &msgs {
            assert_eq!(&decode::<FromDaemon>(&encode(msg)).unwrap(), msg);
        }
    }

    #[test]
    fn empty_fields_stay_off_the_wire() {
        let line = encode(&FromDaemon::StatusChanged {
            status: Status::default(),
        });
        assert!(line.contains("\"state\":\"idle\""));
        assert!(!line.contains("server"));
        assert!(!line.contains("detail"));
        assert!(!line.contains("since_unix"));
    }

    #[test]
    fn unknown_ops_are_rejected() {
        assert!(decode::<ToDaemon>("{\"op\":\"nope\",\"id\":1}").is_err());
        assert!(decode::<FromDaemon>("{\"t\":\"nope\"}").is_err());
        // a bare string or array is not a message
        assert!(decode::<ToDaemon>("\"status\"").is_err());
    }
}
