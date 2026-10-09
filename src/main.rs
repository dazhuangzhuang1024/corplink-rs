mod api;
mod client;
mod config;
mod dns;
mod ipc_server;
mod logbuf;
mod qrcode;
mod resp;
mod state;
mod template;
mod totp;
mod ui;
mod utils;
mod wg;

#[cfg(windows)]
use is_elevated;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use dns::DNSManager;

use std::env;
use std::path::PathBuf;
use std::process::exit;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};

use corplink_ipc::{RunState, Status, VpnServer};
use tokio::sync::{mpsc, oneshot, watch};

use client::Client;
use config::{Config, WgConf};
use ipc_server::{Command, DaemonShared};
use ui::{LoginBridge, LoginUi, PromptError};

fn print_usage_and_exit(name: &str) -> ! {
    println!(
        "usage:\n\t{name} [[--config] config.json]\n\n\
         without a config path, ~/.config/corplink-rs/config.json is used"
    );
    exit(1)
}

/// The config path from the command line: `corplink-rs [[--config] PATH]`;
/// `Ok(None)` means none was given. `Err` asks for the usage text.
fn parse_args(args: &[String]) -> Result<Option<String>, ()> {
    match args {
        [] => Ok(None),
        [flag] if flag == "-h" || flag == "--help" || flag == "--config" => Err(()),
        [flag, path] if flag == "--config" => Ok(Some(path.clone())),
        [path] => Ok(Some(path.clone())),
        _ => Err(()),
    }
}

fn parse_arg() -> Result<String> {
    let args: Vec<String> = env::args().collect();
    let Ok(config) = parse_args(&args[1..]) else {
        print_usage_and_exit(&args[0]);
    };
    if let Some(config) = config {
        return Ok(config);
    }
    let path = config::default_config_path()?;
    if !path.exists() {
        bail!(
            "no config file at {}: create it (config/config.json in the source tree is an \
             example) or pass a config path",
            path.display()
        );
    }
    path.into_os_string()
        .into_string()
        .map_err(|path| anyhow!("the config path {path:?} is not valid UTF-8; pass a config path"))
}

pub const EPERM: i32 = 1;
pub const ENOENT: i32 = 2;
pub const ETIMEDOUT: i32 = 110;
/// The exit code of a restart asked for over IPC: any failure makes launchd
/// (KeepAlive with SuccessfulExit = false) start the daemon again. EX_TEMPFAIL.
const EX_RESTART: i32 = 75;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        log::error!("{:#}", err);
        exit(EPERM);
    }
}

async fn run() -> Result<()> {
    // NOTE: If you want to debug, you should set `RUST_LOG` env to `debug` and run corplink-rs in root
    //  because `check_privilege` will call sudo and drop env if you're not root
    let logs = logbuf::init().context("failed to init the logger")?;

    print_version();

    let conf_file = parse_arg()?;
    let mut conf = Config::from_file(&conf_file)
        .await
        .context("failed to load config")?;
    let name = conf
        .interface_name
        .clone()
        .context("interface name missing in config")?;
    let socks5_listen = conf.socks5_listen.clone();
    let socks5_username = conf.socks5_username.clone().unwrap_or_default();
    let socks5_password = conf.socks5_password.clone().unwrap_or_default();
    let netstack_mode = socks5_listen.is_some();

    // netstack/socks5 mode runs entirely in userspace (no kernel TUN device,
    // no system routes/dns), so it does not require elevated privileges.
    if !netstack_mode {
        check_privilege();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let use_vpn_dns = conf.use_vpn_dns.unwrap_or(false);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let dns_backup_filename = conf.dns_backup_filename.clone();
    // macOS per-domain split DNS (/etc/resolver), independent of use_vpn_dns:
    // the system-wide DNS settings stay untouched either way.
    #[cfg(target_os = "macos")]
    let split_dns_domains = conf.split_dns_domains.clone().unwrap_or_default();
    #[cfg(target_os = "macos")]
    let resolver_files = dns::ResolverFiles::new(dns::RESOLVER_DIR, &name);
    // clean up after earlier runs that never got to remove their files
    // (SIGKILL, crash, power loss), even if split dns has been switched off since
    #[cfg(target_os = "macos")]
    if !netstack_mode {
        if let Err(err) = resolver_files.remove_stale(dns::interface_exists) {
            log::warn!("{:#}", err);
        }
    }
    #[cfg(target_os = "macos")]
    if netstack_mode && !split_dns_domains.is_empty() {
        log::warn!("split_dns_domains is ignored in socks5/netstack mode");
    }
    #[cfg(not(target_os = "macos"))]
    let split_dns_domains: Vec<String> = Vec::new();
    #[cfg(not(target_os = "macos"))]
    if conf
        .split_dns_domains
        .as_ref()
        .is_some_and(|d| !d.is_empty())
    {
        log::warn!("split_dns_domains is only supported on macOS, ignoring it");
    }

    if conf.server.is_none() {
        let resp = client::get_company_url(conf.company_name.as_str())
            .await
            .with_context(|| {
                format!(
                    "failed to fetch company server from company name {}",
                    conf.company_name
                )
            })?;
        log::info!(
            "company name is {}(zh)/{}(en) server is {}",
            resp.zh_name,
            resp.en_name,
            resp.domain
        );
        conf.server = Some(resp.domain);
        conf.save()
            .await
            .context("failed to persist company server")?;
    }

    // Runtime-only override. It goes to a `#[serde(skip)]` field rather than to
    // `vpn_server_name`, because later `Config::save` calls (login state changes)
    // serialize the whole config and would otherwise persist this environment
    // setting into the user's config file.
    if let Ok(vpn_server_name) = env::var("CORPLINK_VPN_SERVER_NAME") {
        if !vpn_server_name.is_empty() {
            log::info!(
                "selecting VPN server {:?} from environment",
                vpn_server_name
            );
            conf.vpn_server_name_override = Some(vpn_server_name);
        }
    }

    // A crate-level debug filter should also expose wireguard-go's handshake
    // and transport logs, without requiring a config-file change.
    let with_wg_log = conf.debug_wg.unwrap_or_default() || log::log_enabled!(log::Level::Debug);
    let platform = conf.platform.clone();

    // the interactive login surface: a terminal for foreground runs, the GUI
    // over IPC for the daemon (stdin under launchd is /dev/null)
    let bridge = Arc::new(LoginBridge::new());
    let login_ui = if utils::stdin_is_usable() {
        LoginUi::terminal()
    } else {
        LoginUi::ipc(bridge.clone())
    };
    let c = Client::new(conf, login_ui).context("failed to initialize client")?;
    let client = Arc::new(tokio::sync::Mutex::new(c));

    // which local user may drive this daemon over ipc besides root: the
    // config file's owner
    #[cfg(unix)]
    let allowed_uid = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&conf_file).map(|m| m.uid()).unwrap_or_else(|_| {
            // SAFETY: geteuid cannot fail
            unsafe { libc::geteuid() }
        })
    };
    #[cfg(not(unix))]
    let allowed_uid = 0;

    let params = SessionParams {
        name: name.clone(),
        netstack_mode,
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        use_vpn_dns,
        split_dns_domains,
        socks5_listen,
        socks5_username,
        socks5_password,
        with_wg_log,
        platform,
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        dns_backup_filename,
    };

    let (status_tx, _) = watch::channel(Status {
        state: RunState::Connecting,
        version: env!("CARGO_PKG_VERSION").to_string(),
        split_dns_domains: params.split_dns_domains.clone(),
        ..Status::default()
    });
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let shared = Arc::new(DaemonShared {
        status: status_tx,
        logs,
        bridge: bridge.clone(),
        cmd_tx,
        allowed_uid,
    });
    // the GUI and the ipc peer check exist on macOS only. Elsewhere no GUI
    // could ever answer a login prompt, so there is no socket, and the daemon
    // keeps failing fast for systemd to see, as it always did.
    let ipc_path = if cfg!(target_os = "macos") {
        match ipc_server::serve(shared.clone()).await {
            Ok(path) => Some(path),
            Err(e) => {
                log::warn!("failed to start the ipc server, continuing without the GUI: {e:#}");
                None
            }
        }
    } else {
        None
    };

    supervise(shared, client, bridge, params, conf_file, cmd_rx, ipc_path).await
}

// ----------------------------------------------------------------- supervisor

/// Everything a session needs that is fixed for the process lifetime.
#[derive(Clone)]
struct SessionParams {
    name: String,
    netstack_mode: bool,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use_vpn_dns: bool,
    split_dns_domains: Vec<String>,
    socks5_listen: Option<String>,
    socks5_username: String,
    socks5_password: String,
    with_wg_log: bool,
    platform: Option<String>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    dns_backup_filename: Option<String>,
}

/// A running VPN session: everything teardown has to undo.
struct ActiveSession {
    wg_conf: WgConf,
    since_unix: i64,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    dns_manager: DNSManager,
    #[cfg(target_os = "macos")]
    resolver_files: dns::ResolverFiles,
}

/// The session phases reported over IPC.
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Connecting,
    Online,
    LoginNeeded,
}

impl From<Phase> for RunState {
    fn from(phase: Phase) -> RunState {
        match phase {
            Phase::Idle => RunState::Idle,
            Phase::Connecting => RunState::Connecting,
            Phase::Online => RunState::Online,
            Phase::LoginNeeded => RunState::LoginNeeded,
        }
    }
}

/// How often a failed auto-connect is retried before giving up to Idle.
const MAX_AUTO_RETRIES: u32 = 5;

/// The first auto-retry wait is RETRY_BACKOFF_BASE_SECS, doubling up to
/// RETRY_BACKOFF_CAP_SECS.
const RETRY_BACKOFF_BASE_SECS: u64 = 10;
const RETRY_BACKOFF_CAP_SECS: u64 = 300;

/// How long an unattended failure waits for a GUI that may be attaching right
/// now (it races the first network failure) before the process exits.
const GUI_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// libwg cannot restart netstack mode within a process: stopWg leaves the
/// SOCKS5 listener of the stopped session bound, so every later start fails
/// with "address already in use". A netstack tunnel lives as long as the
/// process, as it always did.
const NETSTACK_NO_RESTART: &str =
    "netstack (socks5) mode cannot restart the tunnel in-process; restart corplink-rs instead";

fn retry_backoff(failures: u32) -> std::time::Duration {
    let shift = failures.saturating_sub(1).min(5);
    let secs = RETRY_BACKOFF_BASE_SECS
        .saturating_mul(1 << shift)
        .min(RETRY_BACKOFF_CAP_SECS);
    std::time::Duration::from_secs(secs)
}

/// Whether the error chain carries this exact prompt failure.
fn has_prompt_error(err: &anyhow::Error, want: PromptError) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<PromptError>())
        .any(|found| *found == want)
}

fn remove_socket(ipc_path: &Option<PathBuf>) {
    if let Some(path) = ipc_path {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether launchd started this process, so that exiting restarts it. A
/// foreground run whose terminal went away is adopted by launchd too, but
/// nothing starts that one again; it is not worth telling apart.
fn run_by_launchd() -> bool {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: getppid cannot fail
        unsafe { libc::getppid() == 1 }
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// The long-lived session loop. Without a GUI ever connected it behaves like
/// the old straight-line daemon: auto-connect, exit non-zero on failure so
/// launchd restarts us, exit ETIMEDOUT when the handshake dies. Once a GUI has
/// connected, the process instead idles between sessions, waits for login
/// input, and reconnects in place.
#[allow(clippy::too_many_arguments)]
async fn supervise(
    shared: Arc<DaemonShared>,
    client: Arc<tokio::sync::Mutex<Client>>,
    bridge: Arc<LoginBridge>,
    params: SessionParams,
    conf_file: String,
    mut cmd_rx: mpsc::Receiver<Command>,
    ipc_path: Option<PathBuf>,
) -> Result<()> {
    // whether a GUI could ever attach to answer a login prompt
    let gui_possible = ipc_path.is_some();
    let mut phase = Phase::Connecting;
    let mut active: Option<ActiveSession> = None;
    // the network half of connecting; abortable because it owns no local
    // resources (bring_up runs inline below precisely so an abort can never
    // leave a half-started tunnel behind)
    let mut connect_task: Option<tokio::task::JoinHandle<Result<WgConf>>>;
    // completes when the tunnel stops handshaking
    let mut watchdog: Option<tokio::task::JoinHandle<()>> = None;
    let mut failures: u32 = 0;
    let mut detail: Option<String> = None;
    // the server the user pinned for this session; retries keep it, only a
    // connect without a server goes back to the configured selection
    let mut pinned_server: Option<String> = None;
    // set once a netstack tunnel was started, see NETSTACK_NO_RESTART
    let mut netstack_spent = false;

    let publish = |phase: Phase, active: Option<&ActiveSession>, detail: Option<String>| {
        let mut status = Status {
            state: phase.into(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            split_dns_domains: params.split_dns_domains.clone(),
            detail,
            ..Status::default()
        };
        if let Some(session) = active {
            status.server = Some(session.wg_conf.server_name.clone());
            status.since_unix = Some(session.since_unix);
            status.dns = Some(session.wg_conf.dns.clone());
            status.protocol =
                Some(if session.wg_conf.protocol == 1 { "tcp" } else { "udp" }.to_string());
            status.server_split_dns_domains = session.wg_conf.server_split_dns.clone();
        }
        shared.update_status(status);
    };
    publish(phase, None, detail.clone());

    // listened to by one future for the whole loop: tokio drops a signal
    // nobody listens for, and its handler replaces the default action, so a
    // listener made anew per iteration would miss a SIGTERM that arrives
    // while a branch runs (a teardown, the wait for a GUI), and launchd would
    // SIGKILL us without any cleanup
    let shutdown = wait_for_shutdown_signal();
    tokio::pin!(shutdown);

    // auto-connect at startup, exactly as the daemon always did
    connect_task = Some(tokio::spawn(connect_session(client.clone(), None)));

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                log::info!("shutting down");
                stop_all(&mut connect_task, &mut watchdog, &mut active, &bridge, &params, &client)
                    .await;
                remove_socket(&ipc_path);
                log::info!("reach exit");
                exit(0);
            }

            // a closed channel only disables this branch; awaiting anything in
            // the body instead would stall the whole loop, signals included
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    Command::Connect { reply, .. } | Command::Disconnect { reply }
                        if netstack_spent =>
                    {
                        // keep the tunnel that works rather than lose it
                        detail = Some(NETSTACK_NO_RESTART.to_string());
                        publish(phase, active.as_ref(), detail.clone());
                        let _ = reply.send(Err(NETSTACK_NO_RESTART.to_string()));
                    }
                    Command::Connect { server, reply } => {
                        // connect doubles as reconnect: an in-flight attempt
                        // (possibly just a sleeping retry) is aborted, and a
                        // running session is torn down first
                        stop_all(&mut connect_task, &mut watchdog, &mut active, &bridge, &params, &client)
                            .await;
                        if let Some(server) = &server {
                            log::info!("selecting VPN server {server:?} from the GUI");
                        }
                        pinned_server = server;
                        phase = Phase::Connecting;
                        failures = 0;
                        detail = None;
                        publish(phase, None, detail.clone());
                        connect_task = Some(tokio::spawn(connect_session(
                            client.clone(),
                            pinned_server.clone(),
                        )));
                        let _ = reply.send(Ok(()));
                    }
                    Command::Disconnect { reply } => {
                        stop_all(&mut connect_task, &mut watchdog, &mut active, &bridge, &params, &client)
                            .await;
                        phase = Phase::Idle;
                        failures = 0;
                        detail = None;
                        publish(phase, None, detail.clone());
                        let _ = reply.send(Ok(()));
                    }
                    Command::Servers { reply } => {
                        // a throwaway client: the session's client may sit in
                        // a minutes-long login while this probes away. It
                        // reads the config from disk and never writes there.
                        let conf_file = conf_file.clone();
                        tokio::spawn(probe_servers(conf_file, reply));
                    }
                    Command::Restart { reply } => {
                        if !run_by_launchd() {
                            let _ = reply.send(Err(
                                "corplink-rs was not started by launchd, which would start it \
                                 again; restart it yourself"
                                    .to_string(),
                            ));
                            continue;
                        }
                        log::info!("restarting to read the config again");
                        stop_all(&mut connect_task, &mut watchdog, &mut active, &bridge, &params, &client)
                            .await;
                        // the session is gone: say so, and take no new
                        // connections on the way out
                        publish(
                            Phase::Connecting,
                            None,
                            Some("restarting to read the config again".to_string()),
                        );
                        remove_socket(&ipc_path);
                        let _ = reply.send(Ok(()));
                        // the reply goes out from the connection's own task
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        log::info!("reach exit");
                        exit(EX_RESTART);
                    }
                }
            }

            res = async {
                match connect_task.as_mut() {
                    Some(task) => Some(task.await.expect("the connect task must not panic")),
                    None => std::future::pending().await,
                }
            }, if connect_task.is_some() => {
                connect_task = None;
                // the branch only runs with a task in flight
                let res = res.expect("a finished connect task");
                // the local half runs inline: nothing can abort it halfway
                let res = match res {
                    Ok(wg_conf) => {
                        netstack_spent |= params.netstack_mode;
                        bring_up(&params, wg_conf)
                            .await
                            .context("failed to bring the tunnel up")
                    }
                    Err(e) => Err(e),
                };
                match res {
                    Ok(session) => {
                        failures = 0;
                        detail = None;
                        phase = Phase::Online;
                        let watch_name = params.name.clone();
                        watchdog = Some(tokio::spawn(async move {
                            let mut uapi = wg::UAPIClient { name: watch_name };
                            uapi.check_wg_connection().await;
                        }));
                        publish(phase, Some(&session), detail.clone());
                        active = Some(session);
                    }
                    Err(e) => {
                        log::warn!("failed to connect: {e:#}");
                        let summary = format!("{e:#}");
                        let unanswered = has_prompt_error(&e, PromptError::NoUi)
                            || has_prompt_error(&e, PromptError::TimedOut);
                        if has_prompt_error(&e, PromptError::Cancelled) {
                            // the user pressed cancel
                            phase = Phase::Idle;
                            detail = None;
                        } else if unanswered && gui_possible {
                            // a login only a human can provide: wait for one,
                            // even without a GUI yet, instead of exiting
                            // (and thrashing under launchd's restart) or
                            // retrying (an email login mails a fresh code
                            // every time); the GUI starts it when it opens
                            phase = Phase::LoginNeeded;
                            detail = Some(summary);
                        } else if netstack_spent
                            || !bridge.ever_had_gui()
                                && !(gui_possible && bridge.wait_for_gui(GUI_GRACE).await)
                        {
                            // the unattended daemon, or a netstack tunnel
                            // that cannot come back in this process: exit
                            // and let launchd restart us, exactly as before
                            // the GUI existed
                            remove_socket(&ipc_path);
                            return Err(e);
                        } else if failures < MAX_AUTO_RETRIES {
                            failures += 1;
                            let wait = retry_backoff(failures);
                            log::warn!("retry {} in {}s", failures, wait.as_secs());
                            phase = Phase::Connecting;
                            detail = Some(format!(
                                "retry {} in {}s: {}",
                                failures,
                                wait.as_secs(),
                                summary
                            ));
                            let retry_client = client.clone();
                            let retry_pin = pinned_server.clone();
                            connect_task = Some(tokio::spawn(async move {
                                tokio::time::sleep(wait).await;
                                connect_session(retry_client, retry_pin).await
                            }));
                        } else {
                            phase = Phase::Idle;
                            detail = Some(summary);
                        }
                        publish(phase, None, detail.clone());
                    }
                }
            }

            _ = async {
                match watchdog.as_mut() {
                    Some(task) => task.await.expect("the watchdog must not panic"),
                    None => std::future::pending().await,
                }
            }, if watchdog.is_some() => {
                watchdog = None;
                log::warn!("last handshake timeout");
                if let Some(session) = active.take() {
                    teardown(&params, session, &client).await;
                }
                if !bridge.ever_had_gui() || netstack_spent {
                    // exit and let launchd restart us, as before the GUI; a
                    // netstack tunnel cannot come back in this process anyway
                    remove_socket(&ipc_path);
                    exit(ETIMEDOUT);
                }
                // attended: reconnect in place, skipping launchd's restart
                // throttle
                phase = Phase::Connecting;
                detail = Some("reconnecting after the handshake timed out".to_string());
                publish(phase, None, detail.clone());
                connect_task = Some(tokio::spawn(connect_session(
                    client.clone(),
                    pinned_server.clone(),
                )));
            }
        }
    }
}

/// Stops whatever runs: an in-flight connect with its prompt, and the session
/// with its watchdog.
async fn stop_all(
    connect_task: &mut Option<tokio::task::JoinHandle<Result<WgConf>>>,
    watchdog: &mut Option<tokio::task::JoinHandle<()>>,
    active: &mut Option<ActiveSession>,
    bridge: &LoginBridge,
    params: &SessionParams,
    client: &Arc<tokio::sync::Mutex<Client>>,
) {
    if let Some(task) = connect_task.take() {
        task.abort();
        // the aborted task withdraws its prompt once it is dropped; do it
        // now, so no GUI answers it in the meantime
        bridge.cancel();
    }
    if let Some(task) = watchdog.take() {
        task.abort();
    }
    if let Some(session) = active.take() {
        teardown(params, session, client).await;
    }
}

/// The network half of a session: log in if needed and negotiate the wg conf.
/// Safe to abort: it owns no local resources.
async fn connect_session(
    client: Arc<tokio::sync::Mutex<Client>>,
    pin_server: Option<String>,
) -> Result<WgConf> {
    let mut c = client.lock().await;
    let mut logout_retry = true;
    loop {
        if c.need_login() {
            log::info!("not login yet, try to login");
            c.login().await.context("login failed")?;
            log::info!("login success");
        }
        c.set_vpn_server_override(pin_server.clone());
        log::info!("try to connect");
        match c.connect_vpn().await {
            Ok(conf) => return Ok(conf),
            Err(e) => {
                if logout_retry && e.to_string().contains("logout") {
                    // e contains detail message, so just print it out
                    log::warn!("{}", e);
                    logout_retry = false;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// The local half: start wireguard, apply dns and split-dns. Runs to
/// completion once started — the supervisor runs it inline so no abort can
/// leave a half-started tunnel; on error it undoes its own steps.
async fn bring_up(params: &SessionParams, wg_conf: WgConf) -> Result<ActiveSession> {
    let name = params.name.as_str();

    if let Some(listen) = &params.socks5_listen {
        log::info!("start wg-corplink (netstack/socks5) on {}", listen);
        let started = wg::start_wg_go_netstack(
            &wg_conf,
            listen,
            &params.socks5_username,
            &params.socks5_password,
            params.with_wg_log,
        )
        .context("failed to start wg-corplink in netstack mode");
        if let Err(e) = started {
            wg::stop_wg_go();
            return Err(e);
        }
        let mut uapi = wg::UAPIClient {
            name: name.to_string(),
        };
        let configured = uapi
            .config_wg_netstack(&wg_conf)
            .await
            .with_context(|| format!("failed to config netstack interface with uapi for {name}"));
        if let Err(e) = configured {
            wg::stop_wg_go();
            return Err(e);
        }
        if params.socks5_username.is_empty() {
            log::info!("socks5 proxy ready at {} (no auth)", listen);
        } else {
            log::info!(
                "socks5 proxy ready at {} (username/password auth required)",
                listen
            );
        }
    } else {
        log::info!("start wg-corplink for {}", name);
        let started = wg::start_wg_go(name, wg_conf.protocol, params.with_wg_log)
            .with_context(|| format!("failed to start wg-corplink for {}", name));
        if let Err(e) = started {
            wg::stop_wg_go();
            return Err(e);
        }
        let mut uapi = wg::UAPIClient {
            name: name.to_string(),
        };
        let configured = uapi
            .config_wg(&wg_conf)
            .await
            .with_context(|| format!("failed to config interface with uapi for {name}"));
        if let Err(e) = configured {
            wg::stop_wg_go();
            return Err(e);
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let mut dns_manager = DNSManager::new(params.dns_backup_filename.clone());

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if params.use_vpn_dns && !params.netstack_mode {
        match dns_manager.set_dns(vec![&wg_conf.dns], vec![]) {
            Ok(_) => {}
            Err(err) => {
                log::warn!("failed to set dns: {}", err);
            }
        }
    }

    // the DNS assigned by the VPN can change between sessions, so the
    // /etc/resolver files must be rewritten with this session's value on
    // every connect; they are removed again on shutdown
    #[cfg(target_os = "macos")]
    let mut resolver_files = dns::ResolverFiles::new(dns::RESOLVER_DIR, name);
    #[cfg(target_os = "macos")]
    if !params.netstack_mode && !params.split_dns_domains.is_empty() {
        // the listed domains only resolve while their DNS server is reached
        // through the tunnel
        for server in dns::split_dns_servers(&wg_conf.dns) {
            if let Ok(ip) = server.parse() {
                if !utils::cidrs_contain_ip(&wg_conf.allowed_ips, ip) {
                    log::warn!(
                        "split dns: DNS server {} is outside the tunnel's allowed IPs, \
                         lookups for split_dns_domains will not go through the VPN",
                        server
                    );
                }
            }
        }
        // the marker names the interface, and the next start's stale check
        // relies on that being the real device name
        if !dns::interface_exists(name) {
            log::warn!(
                "split dns: no interface named {:?} after starting the tunnel, \
                 use interface_name of the form utunN; no resolver files written",
                name
            );
        } else if let Err(err) = resolver_files.write(&wg_conf.dns, &params.split_dns_domains) {
            log::warn!("{:#}", err);
        }
    }

    Ok(ActiveSession {
        wg_conf,
        since_unix: chrono::Utc::now().timestamp(),
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        dns_manager,
        #[cfg(target_os = "macos")]
        resolver_files,
    })
}

/// Undo a session, in the same order the old straight-line shutdown used.
async fn teardown(
    params: &SessionParams,
    mut session: ActiveSession,
    client: &Arc<tokio::sync::Mutex<Client>>,
) {
    // split dns goes first: it is useless without the tunnel, and the network
    // calls below can take a while to time out once the link is gone
    #[cfg(target_os = "macos")]
    if let Err(err) = session.resolver_files.remove() {
        log::warn!("{:#}", err);
    }

    log::info!("disconnecting vpn...");
    {
        let mut c = client.lock().await;
        if let Err(e) = c.disconnect_vpn(&session.wg_conf).await {
            log::warn!("failed to disconnect vpn: {}", e)
        };

        // only logout for feilian_v1
        if params.platform.as_deref() == Some(config::PLATFORM_CORPLINK_V1) {
            log::info!("logging out current terminal...");
            if let Err(e) = c.logout().await {
                log::warn!("failed to logout: {}", e)
            };
        }
    }

    wg::stop_wg_go();

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if params.use_vpn_dns && !params.netstack_mode {
        match session.dns_manager.restore_dns() {
            Ok(_) => {}
            Err(err) => {
                log::warn!("failed to delete dns: {}", err);
            }
        }
    }
}

/// Answer a `servers` request with a fresh probe round. Loads the config
/// from disk so the probe always sees the newest login state; the probe
/// client never writes it, or the cookies, back.
async fn probe_servers(conf_file: String, reply: oneshot::Sender<Result<Vec<VpnServer>, String>>) {
    let result = async {
        let conf = Config::from_file(&conf_file)
            .await
            .context("failed to load the config for probing")?;
        let mut probe = Client::new_probe(conf).context("failed to init the probe client")?;
        let list = probe.probe_vpn_list().await.context("failed to probe")?;
        Ok::<Vec<VpnServer>, anyhow::Error>(
            list.into_iter()
                .map(|(vpn, latency_ms)| VpnServer {
                    name: vpn.name,
                    en_name: vpn.en_name,
                    protocol: if vpn.protocol_mode == 1 { "tcp" } else { "udp" }.to_string(),
                    latency_ms,
                })
                .collect(),
        )
    }
    .await;
    let _ = reply.send(result.map_err(|e| format!("{e:#}")));
}

// Resolve when the process is asked to terminate: ctrl+c (SIGINT) or, on unix,
// SIGTERM (sent by `docker stop`, systemd, `kill`, etc). Handling SIGTERM lets
// the graceful shutdown path run — notably the feilian_v1 logout that releases
// the server-side terminal slot, which is otherwise leaked on every stop.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => Some(s),
            Err(e) => {
                log::warn!("failed to install SIGTERM handler: {}", e);
                None
            }
        };
        tokio::select! {
            r = tokio::signal::ctrl_c() => {
                if let Err(e) = r {
                    log::warn!("failed to receive signal: {}", e);
                }
                log::info!("ctrl+c received");
            }
            _ = async {
                match term.as_mut() {
                    Some(t) => { t.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                log::info!("SIGTERM received");
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(e) = tokio::signal::ctrl_c().await {
            log::warn!("failed to receive signal: {}", e);
        }
        log::info!("ctrl+c received");
    }
}

fn check_privilege() {
    #[cfg(unix)]
    match sudo::escalate_if_needed() {
        Ok(_) => {}
        Err(_) => {
            log::error!("please run as root");
            exit(EPERM);
        }
    }

    #[cfg(windows)]
    if !is_elevated::is_elevated() {
        log::error!("please run as administrator");
        exit(EPERM);
    }
}

fn print_version() {
    let pkg_name = env!("CARGO_PKG_NAME");
    let pkg_version = env!("CARGO_PKG_VERSION");
    log::info!("running {}@{}", pkg_name, pkg_version);
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn the_config_path_comes_alone_or_after_config() {
        assert_eq!(parse_args(&args(&[])), Ok(None));
        assert_eq!(
            parse_args(&args(&["/etc/corplink/config.json"])),
            Ok(Some("/etc/corplink/config.json".to_string()))
        );
        assert_eq!(
            parse_args(&args(&["--config", "a.json"])),
            Ok(Some("a.json".to_string()))
        );
        for bad in [
            &["-h"][..],
            &["--help"],
            &["--config"],
            &["a.json", "b.json"],
            &["--config", "a.json", "b.json"],
        ] {
            assert_eq!(parse_args(&args(bad)), Err(()), "{bad:?}");
        }
    }
}
