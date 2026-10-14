//! Installing and managing the corplink-rs service from the GUI, so that the
//! app alone is enough on a Mac: the .app carries the daemon and the launchd
//! scripts the command line install uses, the setup window writes the config
//! file, and the scripts run as root behind the system's administrator
//! prompt. Both ways install the very same launchd job.
//!
//! Everything here runs outside the UI thread or is quick (file reads); the
//! decisions are pure functions so they can be tested without a Mac's admin
//! prompt.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Map, Value};

/// The launchd job of the service, written by the install script.
pub const DAEMON_PLIST: &str = "/Library/LaunchDaemons/com.github.pinkd.corplink-rs.plist";

/// The login item that starts the GUI, a per-user launch agent.
const LOGIN_AGENT_LABEL: &str = "com.github.pinkd.corplink-rs.gui";

/// macOS only creates `utunN` devices (N at most 32767), and split DNS needs
/// the name to be fixed, so a new config gets this one.
const DEFAULT_INTERFACE: &str = "utun12345";

// ------------------------------------------------------------------ the app

/// What a packaged app carries to install the service.
#[derive(Debug, Clone, PartialEq)]
pub struct Bundle {
    pub app: PathBuf,
    pub daemon: PathBuf,
    pub install_script: PathBuf,
    pub uninstall_script: PathBuf,
}

impl Bundle {
    /// The bundle the running GUI came in; `None` for a bare `cargo run`.
    pub fn current() -> Option<Bundle> {
        Bundle::from_exe(&std::env::current_exe().ok()?)
    }

    /// `<app>/Contents/MacOS/corplink-gui` → the app, if it has the daemon
    /// and the scripts next to it.
    fn from_exe(exe: &Path) -> Option<Bundle> {
        let macos = exe.parent()?;
        let contents = macos.parent()?;
        let app = contents.parent()?;
        if app.extension() != Some(OsStr::new("app")) {
            return None;
        }
        let scripts = contents.join("Resources").join("scripts");
        let bundle = Bundle {
            app: app.to_path_buf(),
            daemon: macos.join("corplink-rs"),
            install_script: scripts.join("install-launchd.sh"),
            uninstall_script: scripts.join("uninstall-launchd.sh"),
        };
        [
            &bundle.daemon,
            &bundle.install_script,
            &bundle.uninstall_script,
        ]
        .iter()
        .all(|path| path.is_file())
        .then_some(bundle)
    }

    /// Why nothing should be installed from where the app runs: a mounted
    /// DMG goes away on eject, and macOS runs a downloaded app that was not
    /// moved from a random read-only copy; a login item pointing there would
    /// break.
    pub fn placement_problem(&self) -> Option<&'static str> {
        let app = self.app.to_string_lossy();
        (app.starts_with("/Volumes/") || app.contains("/AppTranslocation/"))
            .then_some("请先把 Corplink GUI 拖到「应用程序」文件夹，再从那里打开它")
    }
}

// -------------------------------------------------------------- the service

/// The config file of the installed service (its plist's argument), `None`
/// inside when it cannot be read; the outer `None` means not installed.
pub fn installed_service() -> Option<Option<PathBuf>> {
    if !Path::new(DAEMON_PLIST).exists() {
        return None;
    }
    let out = Command::new("/usr/bin/plutil")
        .args([
            "-extract",
            "ProgramArguments.1",
            "raw",
            "-o",
            "-",
            DAEMON_PLIST,
        ])
        .output()
        .ok()
        .filter(|out| out.status.success());
    let config = out
        .map(|out| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
        .filter(|path| path.starts_with('/'))
        .map(PathBuf::from);
    Some(config)
}

/// The config file corplink-rs uses when given none, for this user.
pub fn default_config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(corplink_ipc::default_config_file(Path::new(&home)))
}

/// Why running an admin task did not work out.
#[derive(Debug, PartialEq)]
pub enum AdminError {
    /// the user dismissed the password prompt
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdminError::Cancelled => write!(f, "已取消"),
            AdminError::Failed(why) => write!(f, "{why}"),
        }
    }
}

/// Installs the service with the app's daemon and `config`.
pub fn install_service(bundle: &Bundle, config: &Path) -> Result<(), AdminError> {
    run_as_admin(
        "Corplink 要安装 VPN 服务。",
        &[
            OsStr::new("/bin/bash"),
            bundle.install_script.as_os_str(),
            OsStr::new("--binary"),
            bundle.daemon.as_os_str(),
            OsStr::new("--config"),
            config.as_os_str(),
        ],
    )
}

/// Replaces the installed daemon with the app's, keeping the installed
/// config file and options.
pub fn reinstall_service(bundle: &Bundle) -> Result<(), AdminError> {
    run_as_admin(
        "Corplink 要用 App 内置的版本重新安装 VPN 服务。",
        &[
            OsStr::new("/bin/bash"),
            bundle.install_script.as_os_str(),
            OsStr::new("--binary"),
            bundle.daemon.as_os_str(),
        ],
    )
}

/// Removes the service; the config, cookies and logs stay.
pub fn uninstall_service(bundle: &Bundle) -> Result<(), AdminError> {
    run_as_admin(
        "Corplink 要卸载 VPN 服务，VPN 会断开。",
        &[OsStr::new("/bin/bash"), bundle.uninstall_script.as_os_str()],
    )
}

/// Runs the command as root behind the system's administrator prompt. Each
/// argument goes through AppleScript's `quoted form of`, so nothing in a
/// path is ever read by the shell. The scripts are handed to /bin/bash
/// rather than executed: a copy from a download carries the quarantine flag.
fn run_as_admin(prompt: &str, argv: &[&OsStr]) -> Result<(), AdminError> {
    const SCRIPT: [&str; 7] = [
        "on run argv",
        "set cmd to \"\"",
        "repeat with i from 2 to count of argv",
        "set cmd to cmd & quoted form of (item i of argv) & \" \"",
        "end repeat",
        "do shell script cmd with prompt (item 1 of argv) with administrator privileges",
        "end run",
    ];
    let mut command = Command::new("/usr/bin/osascript");
    for line in SCRIPT {
        command.arg("-e").arg(line);
    }
    let out = command
        .arg(prompt)
        .args(argv)
        .output()
        .map_err(|e| AdminError::Failed(format!("无法运行 osascript：{e}")))?;
    if out.status.success() {
        return Ok(());
    }
    Err(admin_error(&String::from_utf8_lossy(&out.stderr)))
}

/// osascript's report, e.g. `0:71: execution error: error: no config (1)`:
/// the script's stderr (lines joined by CR) and its exit code.
fn admin_error(stderr: &str) -> AdminError {
    let text = stderr.trim();
    if text.ends_with("(-128)") {
        return AdminError::Cancelled;
    }
    let text = text
        .split_once("execution error: ")
        .map_or(text, |(_, message)| message);
    let text = match text.rsplit_once(" (") {
        Some((message, code))
            if code
                .strip_suffix(')')
                .is_some_and(|code| code.parse::<i64>().is_ok()) =>
        {
            message
        }
        _ => text,
    };
    AdminError::Failed(text.replace('\r', "\n").trim().to_string())
}

// ---------------------------------------------------------------- the config

/// How the account logs in; `platform` in the config.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Platform {
    /// unset: every method the company offers, in its order
    #[default]
    Auto,
    Feilian,
    FeilianV1,
    Ldap,
    Lark,
    Oidc,
    /// a value the form does not know, kept as it is
    Other(String),
}

impl Platform {
    pub const CHOICES: [Platform; 6] = [
        Platform::Auto,
        Platform::Feilian,
        Platform::FeilianV1,
        Platform::Ldap,
        Platform::Lark,
        Platform::Oidc,
    ];

    pub fn label(&self) -> String {
        match self {
            Platform::Auto => "自动（按企业的登录设置）".to_string(),
            Platform::Feilian => "飞连账号（密码，不填则用邮箱验证码）".to_string(),
            Platform::FeilianV1 => "飞连账号 v1（密码）".to_string(),
            Platform::Ldap => "LDAP（密码）".to_string(),
            Platform::Lark => "飞书扫码".to_string(),
            Platform::Oidc => "OIDC 扫码".to_string(),
            Platform::Other(value) => format!("其他：{value}"),
        }
    }

    fn config_value(&self) -> Option<&str> {
        match self {
            Platform::Auto => None,
            Platform::Feilian => Some("feilian"),
            Platform::FeilianV1 => Some("feilian_v1"),
            Platform::Ldap => Some("ldap"),
            Platform::Lark => Some("lark"),
            Platform::Oidc => Some("OIDC"),
            Platform::Other(value) => Some(value),
        }
    }

    fn from_config(value: Option<&str>) -> Platform {
        match value.unwrap_or_default() {
            "" => Platform::Auto,
            "feilian" => Platform::Feilian,
            "feilian_v1" => Platform::FeilianV1,
            "ldap" => Platform::Ldap,
            "lark" => Platform::Lark,
            "OIDC" => Platform::Oidc,
            other => Platform::Other(other.to_string()),
        }
    }

    pub fn needs_password(&self) -> bool {
        matches!(self, Platform::FeilianV1 | Platform::Ldap)
    }
}

/// The fields the setup window edits; the rest of the config file is left
/// as it is.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AccountForm {
    pub company: String,
    pub username: String,
    pub password: String,
    pub platform: Platform,
    pub vpn_server: String,
    /// `split_dns_domains`, one per line (commas and spaces separate too)
    pub split_dns: String,
}

impl AccountForm {
    pub fn from_config(conf: &Map<String, Value>) -> AccountForm {
        let text = |key: &str| {
            conf.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        AccountForm {
            company: text("company_name"),
            username: text("username"),
            password: text("password"),
            platform: Platform::from_config(conf.get("platform").and_then(Value::as_str)),
            vpn_server: text("vpn_server_name"),
            split_dns: conf
                .get("split_dns_domains")
                .and_then(Value::as_array)
                .map(|domains| {
                    domains
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
        }
    }

    /// What has to be filled in first, if anything.
    pub fn problem(&self) -> Option<String> {
        if self.company.trim().is_empty() {
            Some("请填写企业代码".to_string())
        } else if self.username.trim().is_empty() {
            Some("请填写用户名".to_string())
        } else if self.platform.needs_password() && self.password.is_empty() {
            Some("这种登录方式需要填写密码".to_string())
        } else {
            self.split_dns_domains()
                .err()
                .map(|domain| format!("分流域名无效：{domain}"))
        }
    }

    /// The split DNS domains as the daemon takes them (see
    /// `corplink_ipc::normalize_domain`), each once, in the order given; the
    /// first invalid entry as the error.
    pub fn split_dns_domains(&self) -> Result<Vec<String>, String> {
        let mut domains = Vec::new();
        for entry in split_entries(&self.split_dns) {
            let domain = corplink_ipc::normalize_domain(entry).ok_or_else(|| entry.to_string())?;
            if !domains.contains(&domain) {
                domains.push(domain);
            }
        }
        Ok(domains)
    }
}

/// The entries of the split DNS field: one per line, commas and spaces
/// separate too.
fn split_entries(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|entry| !entry.is_empty())
}

/// One distinct entry of the server's split DNS list, as the settings
/// window shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggested {
    /// as the server sent it, e.g. `*.example.com`
    pub entry: String,
    pub kind: SuggestedKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SuggestedKind {
    /// not in the field yet; `domain` is what the config would get
    New { domain: String },
    /// `domain`, or a parent of it (`by`), is in the field already
    Listed { domain: String, by: String },
    /// cannot become a resolver file
    Invalid,
}

impl SuggestedKind {
    /// What the config would get, for the valid ones.
    pub fn domain(&self) -> Option<&str> {
        match self {
            SuggestedKind::New { domain } | SuggestedKind::Listed { domain, .. } => Some(domain),
            SuggestedKind::Invalid => None,
        }
    }
}

/// The server's split DNS list against the field's content, in the server's
/// order, each domain once. A wildcard `*.example.com` stands for
/// `example.com`: a macOS resolver file covers the subdomains of its domain.
pub fn review_split_dns(current: &str, suggested: &[String]) -> Vec<Suggested> {
    let listed: Vec<String> = split_entries(current)
        .filter_map(corplink_ipc::normalize_domain)
        .collect();
    let mut rows: Vec<Suggested> = Vec::new();
    for entry in suggested.iter().map(|entry| entry.trim()) {
        if entry.is_empty() {
            continue;
        }
        let name = entry.strip_prefix("*.").unwrap_or(entry);
        let kind = match corplink_ipc::normalize_domain(name) {
            None => SuggestedKind::Invalid,
            Some(domain) => match listed.iter().find(|parent| covers(parent, &domain)) {
                Some(parent) => SuggestedKind::Listed {
                    domain,
                    by: parent.clone(),
                },
                None => SuggestedKind::New { domain },
            },
        };
        // each domain once; an invalid entry once as it was sent
        let seen = rows.iter().any(|row| match kind.domain() {
            Some(domain) => row.kind.domain() == Some(domain),
            None => row.kind == SuggestedKind::Invalid && row.entry == entry,
        });
        if !seen {
            rows.push(Suggested {
                entry: entry.to_string(),
                kind,
            });
        }
    }
    rows
}

/// Whether a resolver file for `parent` answers for `domain`: the same
/// domain, or one under it.
fn covers(parent: &str, domain: &str) -> bool {
    domain == parent || domain.ends_with(&format!(".{parent}"))
}

/// What importing the server's split DNS list did.
#[derive(Debug, PartialEq)]
pub struct SplitDnsImport {
    /// the form's field with the new domains appended
    pub text: String,
    pub added: usize,
}

/// Merges the split DNS domains the server suggests into the form's field.
/// A wildcard `*.example.com` becomes `example.com`: a macOS resolver file
/// covers the subdomains of its domain too. Domains already listed, or under
/// a listed parent, are left out; the user's own lines stay as they are.
pub fn import_split_dns(current: &str, suggested: &[String]) -> SplitDnsImport {
    let listed: Vec<String> = split_entries(current)
        .filter_map(corplink_ipc::normalize_domain)
        .collect();
    let mut candidates: Vec<String> = Vec::new();
    for entry in suggested.iter().map(|entry| entry.trim()) {
        let name = entry.strip_prefix("*.").unwrap_or(entry);
        // what cannot be a resolver file is no candidate; the review shows it
        match corplink_ipc::normalize_domain(name) {
            Some(domain) if !candidates.contains(&domain) => candidates.push(domain),
            _ => {}
        }
    }
    let added: Vec<&String> = candidates
        .iter()
        .filter(|domain| {
            !listed.iter().any(|parent| covers(parent, domain))
                && !candidates
                    .iter()
                    .any(|parent| parent != *domain && covers(parent, domain))
        })
        .collect();
    let mut text = current.trim_end().to_string();
    for domain in &added {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(domain);
    }
    SplitDnsImport {
        text,
        added: added.len(),
    }
}

/// Whether the company code has to be looked up before saving: a new or
/// changed company, or a config without its server yet.
pub fn needs_lookup(existing: Option<&Map<String, Value>>, form: &AccountForm) -> bool {
    let Some(conf) = existing else {
        return true;
    };
    let company = conf.get("company_name").and_then(Value::as_str);
    let server = conf
        .get("server")
        .and_then(Value::as_str)
        .unwrap_or_default();
    company != Some(form.company.trim()) || server.is_empty()
}

/// The config after the setup form: the form's fields over what the file
/// had, which keeps everything else (keys, device id, login state, advanced
/// options). Another account starts over with a fresh login.
pub fn merge_config(
    existing: Option<Map<String, Value>>,
    form: &AccountForm,
    server: Option<&str>,
) -> Map<String, Value> {
    let mut conf = existing.unwrap_or_default();
    let company = form.company.trim();
    let username = form.username.trim();
    let same_account = conf.get("company_name").and_then(Value::as_str) == Some(company)
        && conf.get("username").and_then(Value::as_str) == Some(username);
    if !same_account {
        // the login state and the TOTP secret belong to the previous account
        conf.remove("state");
        conf.remove("code");
    }
    conf.insert("company_name".into(), company.into());
    conf.insert("username".into(), username.into());
    set_or_remove(&mut conf, "password", &form.password);
    set_or_remove(
        &mut conf,
        "platform",
        form.platform.config_value().unwrap_or_default(),
    );
    set_or_remove(&mut conf, "vpn_server_name", form.vpn_server.trim());
    // the form refuses to save invalid entries; none left means none at all
    let domains = form.split_dns_domains().unwrap_or_default();
    if domains.is_empty() {
        conf.remove("split_dns_domains");
    } else {
        conf.insert("split_dns_domains".into(), domains.into());
    }
    if let Some(server) = server {
        conf.insert("server".into(), server.into());
    }
    conf.entry("interface_name")
        .or_insert_with(|| DEFAULT_INTERFACE.into());
    conf
}

fn set_or_remove(conf: &mut Map<String, Value>, key: &str, value: &str) {
    if value.is_empty() {
        conf.remove(key);
    } else {
        conf.insert(key.into(), value.into());
    }
}

/// The config file's content, `None` when there is none yet.
pub fn read_config(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("无法读取 {}：{e}", path.display())),
    };
    match serde_json::from_str(&text) {
        Ok(Value::Object(conf)) => Ok(Some(conf)),
        Ok(_) => Err(format!("{} 不是一个 JSON 对象", path.display())),
        Err(e) => Err(format!("无法解析 {}：{e}", path.display())),
    }
}

/// Writes the config file for the owner alone (0600; a directory made here
/// gets 0700): it holds the password and the WireGuard private key. It goes
/// through a temporary file and a rename, so the root daemon never reads half
/// of it.
pub fn write_config(path: &Path, conf: &Map<String, Value>) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("the config path has no directory"))?;
    if !dir.exists() {
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::DirBuilder::new().mode(0o700).create(dir)?;
    }
    let mut text = serde_json::to_string_pretty(conf).map_err(io::Error::other)?;
    text.push('\n');

    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("the config path has no file name"))?;
    // apart from the daemon's own temporary file
    let tmp = dir.join(format!(".{}.gui.tmp", name.to_string_lossy()));
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let written = (|| {
        // create_new also refuses a symlink planted at the temporary name
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// What the company lookup found.
#[derive(Debug, PartialEq)]
pub struct Company {
    pub name: String,
    pub server: String,
}

/// Looks the company code up, the same request the daemon makes.
pub fn lookup_company(code: &str) -> Result<Company, String> {
    // the body starts with `{`, so curl never takes it for an @file
    let body = serde_json::json!({ "code": code.trim() }).to_string();
    let out = Command::new("/usr/bin/curl")
        .args([
            "--silent",
            "--show-error",
            "--max-time",
            "15",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "--data-binary",
        ])
        .arg(&body)
        .arg(corplink_ipc::COMPANY_LOOKUP_URL)
        .output()
        .map_err(|e| format!("无法运行 curl：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "查询企业代码失败（网络不通？）：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    parse_company(&String::from_utf8_lossy(&out.stdout))
}

/// `{"code":0,"data":{"zh_name":…,"domain":"https://…"}}`, or a non-zero
/// code with a message (1005: no such company).
fn parse_company(body: &str) -> Result<Company, String> {
    let resp: Value = serde_json::from_str(body)
        .map_err(|_| format!("企业查询返回了无法识别的内容：{}", body.trim()))?;
    if resp["code"].as_i64() != Some(0) {
        let why = resp["message"].as_str().unwrap_or("unknown error");
        return Err(format!("企业代码无效：{why}"));
    }
    let data = &resp["data"];
    let server = data["domain"]
        .as_str()
        .filter(|server| !server.is_empty())
        .ok_or("企业查询没有返回服务器地址")?;
    let name = ["zh_name", "name", "en_name"]
        .iter()
        .find_map(|key| data[*key].as_str().filter(|name| !name.is_empty()))
        .unwrap_or_default();
    Ok(Company {
        name: name.to_string(),
        server: server.to_string(),
    })
}

/// Shows the config file in Finder, or its directory when it is not there.
pub fn reveal_in_finder(path: &Path) {
    let result = if path.exists() {
        Command::new("/usr/bin/open").arg("-R").arg(path).status()
    } else {
        Command::new("/usr/bin/open")
            .arg(path.parent().unwrap_or(path))
            .status()
    };
    if let Err(e) = result {
        eprintln!("[gui] failed to open Finder: {e}");
    }
}

// -------------------------------------------------------------- login item

fn login_agent_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(
        Path::new(&home)
            .join("Library/LaunchAgents")
            .join(format!("{LOGIN_AGENT_LABEL}.plist")),
    )
}

pub fn login_item_enabled() -> bool {
    login_agent_path().is_some_and(|path| path.exists())
}

/// Starts the GUI at login (a launch agent opening the app), or stops doing
/// so.
pub fn set_login_item(app: &Path, enabled: bool) -> io::Result<()> {
    let path = login_agent_path().ok_or_else(|| io::Error::other("HOME is not set"))?;
    if !enabled {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, login_agent_plist(app))
}

/// Points an enabled login item at where the app is now, e.g. after it was
/// moved or replaced by a newer copy elsewhere.
pub fn refresh_login_item(app: &Path) {
    let Some(path) = login_agent_path() else {
        return;
    };
    match fs::read_to_string(&path) {
        Ok(text) if text != login_agent_plist(app) => {
            if let Err(e) = fs::write(&path, login_agent_plist(app)) {
                eprintln!("[gui] failed to update the login item: {e}");
            }
        }
        _ => {}
    }
}

fn login_agent_plist(app: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LOGIN_AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/open</string>
        <string>-a</string>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
</dict>
</plist>
"#,
        xml_escape(&app.to_string_lossy())
    )
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn form(company: &str, username: &str) -> AccountForm {
        AccountForm {
            company: company.into(),
            username: username.into(),
            ..AccountForm::default()
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("corplink-gui-{tag}-{unique}"))
    }

    #[test]
    fn a_new_config_gets_the_form_server_and_interface() {
        let mut new = form(" acme ", " alice ");
        new.password = "secret".into();
        new.platform = Platform::Ldap;
        let conf = merge_config(None, &new, Some("https://vpn.acme.example"));
        assert_eq!(
            Value::Object(conf),
            json!({
                "company_name": "acme",
                "username": "alice",
                "password": "secret",
                "platform": "ldap",
                "server": "https://vpn.acme.example",
                "interface_name": "utun12345",
            })
        );
    }

    #[test]
    fn saving_the_form_keeps_everything_else() {
        let existing = object(json!({
            "company_name": "acme",
            "username": "alice",
            "password": "old",
            "platform": "lark",
            "vpn_server_name": "Node03",
            "server": "https://vpn.acme.example",
            "interface_name": "utun7",
            "private_key": "k",
            "state": "login",
            "code": "totp",
            "split_dns_domains": ["intra.example"],
        }));
        let mut same = AccountForm::from_config(&existing);
        assert_eq!(same.platform, Platform::Lark);
        assert_eq!(same.split_dns, "intra.example");
        same.password.clear();
        same.platform = Platform::Auto;
        same.vpn_server.clear();
        assert!(!needs_lookup(Some(&existing), &same));

        let conf = merge_config(Some(existing), &same, None);
        // cleared fields go, the login of the same account stays
        assert_eq!(
            Value::Object(conf),
            json!({
                "company_name": "acme",
                "username": "alice",
                "server": "https://vpn.acme.example",
                "interface_name": "utun7",
                "private_key": "k",
                "state": "login",
                "code": "totp",
                "split_dns_domains": ["intra.example"],
            })
        );
    }

    #[test]
    fn another_account_logs_in_afresh() {
        let existing = object(json!({
            "company_name": "acme",
            "username": "alice",
            "server": "https://vpn.acme.example",
            "state": "login",
            "code": "totp",
            "private_key": "k",
        }));
        let bob = form("acme", "bob");
        assert!(!needs_lookup(Some(&existing), &bob));
        let conf = merge_config(Some(existing.clone()), &bob, None);
        assert!(!conf.contains_key("state") && !conf.contains_key("code"));
        assert_eq!(conf["private_key"], "k");

        // another company: looked up again, its server replaces the old one
        let other = form("globex", "alice");
        assert!(needs_lookup(Some(&existing), &other));
        let conf = merge_config(Some(existing), &other, Some("https://vpn.globex.example"));
        assert_eq!(conf["server"], "https://vpn.globex.example");
        assert!(!conf.contains_key("state"));

        // nothing to go on yet
        assert!(needs_lookup(None, &form("acme", "alice")));
        assert!(needs_lookup(
            Some(&object(json!({"company_name": "acme"}))),
            &form("acme", "alice")
        ));
    }

    #[test]
    fn unknown_platforms_survive_the_form() {
        let conf = object(json!({"company_name": "a", "username": "b", "platform": "dingtalk"}));
        let same = AccountForm::from_config(&conf);
        assert_eq!(same.platform, Platform::Other("dingtalk".into()));
        assert_eq!(
            merge_config(Some(conf), &same, None)["platform"],
            "dingtalk"
        );
    }

    #[test]
    fn split_dns_domains_are_checked_like_the_daemon_does() {
        let mut edited = form("acme", "alice");
        edited.split_dns =
            " Intra.Example.COM.\nwiki.example, intra.example.com  cname.example\n\n".into();
        assert_eq!(
            edited.split_dns_domains(),
            Ok(vec![
                "intra.example.com".to_string(),
                "wiki.example".to_string(),
                "cname.example".to_string(),
            ])
        );
        assert_eq!(edited.problem(), None);
        let conf = merge_config(None, &edited, Some("https://vpn.acme.example"));
        assert_eq!(
            conf["split_dns_domains"],
            json!(["intra.example.com", "wiki.example", "cname.example"])
        );

        // a file name under /etc/resolver must stay a plain name
        for bad in ["../etc/passwd", "a/b", "a_b", "a..b"] {
            edited.split_dns = format!("ok.example\n{bad}");
            assert_eq!(edited.split_dns_domains(), Err(bad.to_string()));
            assert!(edited.problem().unwrap().contains(bad));
        }

        // emptied: the key goes
        let existing = object(json!({
            "company_name": "acme",
            "username": "alice",
            "split_dns_domains": ["intra.example"],
        }));
        let mut cleared = AccountForm::from_config(&existing);
        cleared.split_dns = " \n ".into();
        assert!(!merge_config(Some(existing), &cleared, None).contains_key("split_dns_domains"));
    }

    #[test]
    fn the_server_split_dns_list_is_imported_without_repeats() {
        // what the server of the issue's company sends, trimmed
        let suggested: Vec<String> = [
            "gitlab.dev.example",
            "oa.office.example",
            "hids.office.example",
            "*.corp.example",
            "c-0123456789abcdef.db.cloud.example",
            "hids.office.example",
            "*.sec.example",
            "api.sec.example",
            "*.chat.local",
            "under_score.example",
            "*",
            "oa.office.example",
        ]
        .iter()
        .map(|entry| entry.to_string())
        .collect();
        let imported = import_split_dns("corp.example\nlb.cloud.example\n", &suggested);
        assert_eq!(
            imported.text,
            "corp.example\nlb.cloud.example\n\
             gitlab.dev.example\noa.office.example\nhids.office.example\n\
             c-0123456789abcdef.db.cloud.example\nsec.example\nchat.local"
        );
        // *.corp.example is listed already, api.sec.example sits under sec.example
        assert_eq!(imported.added, 6);

        // a second import adds nothing
        let again = import_split_dns(&imported.text, &suggested);
        assert_eq!(
            (again.text.as_str(), again.added),
            (imported.text.as_str(), 0)
        );

        // into an empty field, and what it gives passes the form
        let fresh = import_split_dns("", &["*.Example.COM".to_string()]);
        assert_eq!(fresh.text, "example.com");
        let mut filled = form("acme", "alice");
        filled.split_dns = imported.text;
        assert_eq!(filled.problem(), None);
    }

    #[test]
    fn the_server_split_dns_list_is_shown_entry_by_entry() {
        let suggested: Vec<String> = [
            "gitlab.dev.example",
            "*.corp.example",
            "api.corp.example",
            "hids.office.example",
            "hids.office.example",
            "*.sec.example",
            "under_score.example",
            "under_score.example",
            "  ",
        ]
        .iter()
        .map(|entry| entry.to_string())
        .collect();
        let new = |entry: &str, domain: &str| Suggested {
            entry: entry.into(),
            kind: SuggestedKind::New {
                domain: domain.into(),
            },
        };
        let listed = |entry: &str, domain: &str, by: &str| Suggested {
            entry: entry.into(),
            kind: SuggestedKind::Listed {
                domain: domain.into(),
                by: by.into(),
            },
        };
        let rows = review_split_dns("corp.example\nwiki.example", &suggested);
        assert_eq!(
            rows,
            [
                new("gitlab.dev.example", "gitlab.dev.example"),
                listed("*.corp.example", "corp.example", "corp.example"),
                listed("api.corp.example", "api.corp.example", "corp.example"),
                new("hids.office.example", "hids.office.example"),
                new("*.sec.example", "sec.example"),
                Suggested {
                    entry: "under_score.example".into(),
                    kind: SuggestedKind::Invalid,
                },
            ]
        );

        // what is picked and imported shows as listed afterwards
        let imported = import_split_dns("corp.example\nwiki.example", &["sec.example".to_string()]);
        assert_eq!(imported.text, "corp.example\nwiki.example\nsec.example");
        let after = review_split_dns(&imported.text, &suggested);
        assert_eq!(
            after[4],
            listed("*.sec.example", "sec.example", "sec.example")
        );
        assert_eq!(after[0], new("gitlab.dev.example", "gitlab.dev.example"));
    }

    #[test]
    fn the_form_asks_for_what_is_missing() {
        assert!(form("", "alice").problem().is_some());
        assert!(form("acme", " ").problem().is_some());
        assert_eq!(form("acme", "alice").problem(), None);
        let mut ldap = form("acme", "alice");
        ldap.platform = Platform::Ldap;
        assert!(ldap.problem().is_some());
        ldap.password = "secret".into();
        assert_eq!(ldap.problem(), None);
        // feilian without a password logs in with an email code
        let mut feilian = form("acme", "alice");
        feilian.platform = Platform::Feilian;
        assert_eq!(feilian.problem(), None);
    }

    #[test]
    fn company_lookups_are_parsed() {
        assert_eq!(
            parse_company(
                r#"{"code":0,"data":{"name":"acme","zh_name":"艾克米","en_name":"Acme","domain":"https://vpn.acme.example:10443"}}"#
            ),
            Ok(Company {
                name: "艾克米".into(),
                server: "https://vpn.acme.example:10443".into(),
            })
        );
        let unknown = parse_company(
            r#"{"code":1005,"action":"alert","message":"The enterprise identification code does not exist","data":null}"#,
        )
        .unwrap_err();
        assert!(unknown.contains("does not exist"), "{unknown}");
        assert!(parse_company(r#"{"code":0,"data":{"domain":""}}"#).is_err());
        assert!(parse_company("<html>bad gateway</html>").is_err());
    }

    #[test]
    fn config_files_are_written_privately_and_read_back() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("config");
        let path = dir.join("corplink-rs").join("config.json");
        assert_eq!(read_config(&path), Ok(None));

        let conf = object(json!({"company_name": "acme", "username": "alice"}));
        write_config(&path, &conf).unwrap();
        assert_eq!(read_config(&path), Ok(Some(conf.clone())));
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);

        // a rewrite replaces the file and leaves no temporary file behind
        write_config(&path, &object(json!({"username": "bob"}))).unwrap();
        assert_eq!(read_config(&path).unwrap().unwrap()["username"], "bob");
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.json"]);

        fs::write(&path, "[1, 2]").unwrap();
        assert!(read_config(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_bundle_is_found_next_to_the_gui() {
        let root = temp_dir("bundle");
        let app = root.join("Corplink GUI.app");
        let exe = app.join("Contents/MacOS/corplink-gui");
        // a bare binary is no bundle
        assert_eq!(Bundle::from_exe(&root.join("corplink-gui")), None);
        fs::create_dir_all(app.join("Contents/Resources/scripts")).unwrap();
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        for file in [
            "Contents/MacOS/corplink-gui",
            "Contents/MacOS/corplink-rs",
            "Contents/Resources/scripts/install-launchd.sh",
        ] {
            fs::write(app.join(file), "").unwrap();
        }
        // the uninstall script is missing still
        assert_eq!(Bundle::from_exe(&exe), None);
        fs::write(
            app.join("Contents/Resources/scripts/uninstall-launchd.sh"),
            "",
        )
        .unwrap();
        let bundle = Bundle::from_exe(&exe).unwrap();
        assert_eq!(bundle.app, app);
        assert_eq!(bundle.daemon, app.join("Contents/MacOS/corplink-rs"));
        assert_eq!(bundle.placement_problem(), None);
        fs::remove_dir_all(&root).unwrap();

        let on_dmg = Bundle {
            app: PathBuf::from("/Volumes/Corplink GUI/Corplink GUI.app"),
            ..bundle.clone()
        };
        assert!(on_dmg.placement_problem().is_some());
        let translocated = Bundle {
            app: PathBuf::from("/private/var/folders/x/AppTranslocation/y/d/Corplink GUI.app"),
            ..bundle
        };
        assert!(translocated.placement_problem().is_some());
    }

    #[test]
    fn admin_failures_are_told_apart() {
        assert_eq!(
            admin_error("0:205: execution error: User canceled. (-128)\n"),
            AdminError::Cancelled
        );
        assert_eq!(
            admin_error(
                "0:56: execution error: error: config file not found: /x\rrolling back (1)\n"
            ),
            AdminError::Failed("error: config file not found: /x\nrolling back".into())
        );
        assert_eq!(
            admin_error("something else"),
            AdminError::Failed("something else".into())
        );
    }

    #[test]
    fn the_login_item_opens_the_app_where_it_is() {
        let plist = login_agent_plist(Path::new("/Applications/A & B <x>.app"));
        assert!(plist.contains("<string>/Applications/A &amp; B &lt;x&gt;.app</string>"));
        assert!(plist.contains("<string>/usr/bin/open</string>"));
        assert!(plist.contains(LOGIN_AGENT_LABEL));
    }
}
