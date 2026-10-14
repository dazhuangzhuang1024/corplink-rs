//! The daemon side of the GUI IPC: a Unix domain socket speaking the
//! `corplink_ipc` protocol.
//!
//! Every connected GUI immediately receives the current status and standing
//! login prompt, then events as the world changes. Commands that drive the
//! session (connect/disconnect/server probing) are forwarded to the supervisor
//! through a channel; login answers are delivered directly through the
//! [`LoginBridge`].
//!
//! Connections are accepted only from root or from the user who owns the
//! config file (`LOCAL_PEERCRED`), so other users of a shared machine cannot
//! drive, or read logs from, a root daemon.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, oneshot, watch};

use corplink_ipc::{Status, VpnServer};

use crate::logbuf::LogBuffer;
use crate::ui::LoginBridge;

// the transport is a unix domain socket; elsewhere `serve` only says so
#[cfg(unix)]
use {
    anyhow::{anyhow, bail, Context},
    corplink_ipc::{decode, encode, socket_candidates, FromDaemon, RespData, ToDaemon},
    std::path::Path,
    tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader},
    tokio::net::{UnixListener, UnixStream},
    tokio::sync::broadcast,
};

/// Session commands the IPC server forwards to the supervisor (main.rs).
#[derive(Debug)]
pub enum Command {
    /// Start or restart a session, optionally pinned to one VPN server.
    Connect {
        server: Option<String>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Tear the session down and go idle.
    Disconnect {
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Measure the server list with fresh probes.
    Servers {
        reply: oneshot::Sender<Result<Vec<VpnServer>, String>>,
    },
    /// Stop the session and exit, for launchd to start the daemon again with
    /// the config file as it is now.
    Restart {
        reply: oneshot::Sender<Result<(), String>>,
    },
}

/// Everything the connection handlers need, shared with the supervisor.
pub struct DaemonShared {
    pub status: watch::Sender<Status>,
    pub logs: Arc<LogBuffer>,
    pub bridge: Arc<LoginBridge>,
    pub cmd_tx: mpsc::Sender<Command>,
    /// The uid allowed to connect besides root: the config file's owner.
    pub allowed_uid: u32,
}

impl DaemonShared {
    pub fn update_status(&self, status: Status) {
        // send_replace, so a GUI connecting late still finds the latest value
        self.status.send_replace(status);
    }

    pub fn current_status(&self) -> Status {
        self.status.borrow().clone()
    }
}

/// Binds the first workable socket path and serves connections forever.
/// Returns the bound path (the supervisor removes the file on shutdown).
#[cfg(unix)]
pub async fn serve(shared: Arc<DaemonShared>) -> Result<PathBuf> {
    let mut last_err = None;
    for path in socket_candidates() {
        match serve_at(&path, shared.clone()).await {
            Ok(()) => return Ok(path),
            Err(e) => {
                log::debug!("cannot bind ipc socket at {}: {e}", path.display());
                last_err = Some((path, e));
            }
        }
    }
    let (path, e) = last_err.expect("socket_candidates is never empty");
    Err(anyhow!(
        "failed to bind any ipc socket ({}): {e}",
        path.display()
    ))
}

#[cfg(not(unix))]
pub async fn serve(_shared: Arc<DaemonShared>) -> Result<PathBuf> {
    anyhow::bail!("the GUI ipc needs unix domain sockets")
}

/// Binds exactly `path` and starts serving; fails when another daemon already
/// listens there.
#[cfg(unix)]
pub async fn serve_at(path: &Path, shared: Arc<DaemonShared>) -> Result<()> {
    let listener = bind_at(path).await?;
    log::info!("ipc server listening on {}", path.display());
    tokio::spawn(accept_loop(listener, shared));
    Ok(())
}

#[cfg(unix)]
async fn bind_at(path: &Path) -> Result<UnixListener> {
    match std::fs::metadata(path) {
        Ok(_) => {
            // a leftover socket from a run that did not clean up; a live one
            // means another daemon owns it
            if tokio::net::UnixStream::connect(path).await.is_ok() {
                bail!("another daemon is listening here");
            }
            std::fs::remove_file(path).context("failed to remove the stale socket file")?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("failed to inspect the socket path"),
    }
    let listener = UnixListener::bind(path).context("failed to bind the socket")?;
    // root's umask (launchd sets 0077) would keep the GUI out; access control
    // happens per connection via the peer credentials
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
            .context("failed to loosen the socket permissions")?;
    }
    Ok(listener)
}

#[cfg(unix)]
async fn accept_loop(listener: UnixListener, shared: Arc<DaemonShared>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, shared).await {
                        log::debug!("ipc connection ended: {e:#}");
                    }
                });
            }
            Err(e) => {
                log::warn!("ipc accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

#[cfg(unix)]
async fn handle_conn(stream: UnixStream, shared: Arc<DaemonShared>) -> Result<()> {
    let uid = match peer_euid(&stream) {
        Some(uid) if uid == 0 || uid == shared.allowed_uid => uid,
        Some(uid) => {
            log::warn!("refusing ipc connection from uid {uid}");
            return Ok(());
        }
        None => {
            log::warn!("refusing ipc connection with unknowable peer credentials");
            return Ok(());
        }
    };
    log::info!("gui connected (uid {uid})");
    // nothing may return between attach and detach
    shared.bridge.gui_attach();
    let result = conn_loop(stream, &shared).await;
    shared.bridge.gui_detach();
    log::info!("gui disconnected");
    result
}

#[cfg(unix)]
async fn conn_loop(stream: UnixStream, shared: &Arc<DaemonShared>) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = AsyncBufReader::new(rd).lines();
    let mut status_rx = shared.status.subscribe();
    let mut prompt_rx = shared.bridge.subscribe_prompts();
    // replies to the requests answered aside, see `forward`
    let (replies, mut reply_rx) = mpsc::unbounded_channel();
    // becomes Some on the first `logs` request, together with this
    // connection's own subscription to the live lines
    let mut log_rx: Option<broadcast::Receiver<String>> = None;

    // hand a freshly connected GUI the full picture without it asking.
    // The watch refs are bound first: a temporary inside the awaited call
    // expression would live across the await and make this future !Send.
    let status = shared.current_status();
    send(&mut wr, FromDaemon::StatusChanged { status }).await?;
    let prompt = prompt_rx.borrow_and_update().clone();
    send(&mut wr, FromDaemon::LoginPromptUpdate { prompt }).await?;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = match line.context("failed to read from the gui")? {
                    Some(line) => line,
                    None => return Ok(()), // the gui closed the connection
                };
                if line.trim().is_empty() {
                    continue;
                }
                let msg: ToDaemon = match decode(&line) {
                    Ok(msg) => msg,
                    // one bad line does not take the connection down; the
                    // gui relies on it surviving stray output
                    Err(e) => {
                        log::warn!("ignoring an unparsable gui message ({e}): {line}");
                        continue;
                    }
                };
                handle_request(msg, &mut wr, shared, &mut log_rx, &replies).await?;
            }
            changed = status_rx.changed() => {
                if changed.is_err() {
                    return Ok(()); // the daemon is shutting down
                }
                let status = status_rx.borrow_and_update().clone();
                send(&mut wr, FromDaemon::StatusChanged { status }).await?;
            }
            changed = prompt_rx.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let prompt = prompt_rx.borrow_and_update().clone();
                send(&mut wr, FromDaemon::LoginPromptUpdate { prompt }).await?;
            }
            // never None: this loop holds a sender
            Some(frame) = reply_rx.recv() => send(&mut wr, frame).await?,
            line = async {
                match log_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match line {
                    Ok(line) => send(&mut wr, FromDaemon::LogLine { line }).await?,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // the ring buffer still has the missed lines
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        log_rx.take();
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
async fn handle_request(
    msg: ToDaemon,
    wr: &mut (impl tokio::io::AsyncWrite + Unpin),
    shared: &Arc<DaemonShared>,
    log_rx: &mut Option<broadcast::Receiver<String>>,
    replies: &mpsc::UnboundedSender<FromDaemon>,
) -> Result<()> {
    match msg {
        ToDaemon::Status { id } => {
            reply(
                wr,
                id,
                Ok(Some(RespData::Status {
                    status: shared.current_status(),
                })),
            )
            .await?
        }
        ToDaemon::Logs { id, tail } => {
            // subscribed before taking the tail: a line logged in between
            // then shows up twice rather than not at all
            log_rx.replace(shared.logs.subscribe());
            let lines = shared.logs.tail(tail as usize);
            reply(wr, id, Ok(Some(RespData::Logs { lines }))).await?
        }
        ToDaemon::LoginReply { id, value } => {
            if shared.bridge.reply(value) {
                reply(wr, id, Ok(None)).await?
            } else {
                reply(wr, id, Err("no login prompt is active".to_string())).await?
            }
        }
        ToDaemon::LoginCancel { id } => {
            shared.bridge.cancel();
            reply(wr, id, Ok(None)).await?
        }
        ToDaemon::Connect { id, server } => {
            let (tx, rx) = oneshot::channel();
            let cmd = Command::Connect { server, reply: tx };
            forward(wr, id, shared, cmd, rx, replies, |()| None).await?
        }
        ToDaemon::LoginStart { id } => {
            let (tx, rx) = oneshot::channel();
            let cmd = Command::Connect {
                server: None,
                reply: tx,
            };
            forward(wr, id, shared, cmd, rx, replies, |()| None).await?
        }
        ToDaemon::Disconnect { id } => {
            let (tx, rx) = oneshot::channel();
            let cmd = Command::Disconnect { reply: tx };
            forward(wr, id, shared, cmd, rx, replies, |()| None).await?
        }
        ToDaemon::Servers { id } => {
            let (tx, rx) = oneshot::channel();
            let cmd = Command::Servers { reply: tx };
            forward(wr, id, shared, cmd, rx, replies, |servers| {
                Some(RespData::Servers { servers })
            })
            .await?
        }
        ToDaemon::Restart { id } => {
            let (tx, rx) = oneshot::channel();
            let cmd = Command::Restart { reply: tx };
            forward(wr, id, shared, cmd, rx, replies, |()| None).await?
        }
    }
    Ok(())
}

/// Hands a command to the supervisor and relays its answer. The command is
/// queued right away, in request order; only the wait for the answer runs
/// aside, so a slow one (a teardown, a server probe takes seconds) holds up
/// neither this GUI's events nor its next requests, a login code among them.
#[cfg(unix)]
async fn forward<T: Send + 'static>(
    wr: &mut (impl tokio::io::AsyncWrite + Unpin),
    id: u64,
    shared: &Arc<DaemonShared>,
    cmd: Command,
    rx: oneshot::Receiver<Result<T, String>>,
    replies: &mpsc::UnboundedSender<FromDaemon>,
    data: fn(T) -> Option<RespData>,
) -> Result<()> {
    if shared.cmd_tx.send(cmd).await.is_err() {
        return reply(wr, id, Err("the daemon is shutting down".into())).await;
    }
    let replies = replies.clone();
    tokio::spawn(async move {
        let answer = rx
            .await
            .unwrap_or_else(|_| Err("the supervisor did not answer".into()))
            .map(data);
        // the gui may be gone by now; nobody else wants the answer
        let _ = replies.send(reply_frame(id, answer));
    });
    Ok(())
}

#[cfg(unix)]
fn reply_frame(id: u64, answer: Result<Option<RespData>, String>) -> FromDaemon {
    let (ok, error, data) = match answer {
        Ok(data) => (true, None, data),
        Err(e) => (false, Some(e), None),
    };
    FromDaemon::Reply {
        id,
        ok,
        error,
        data,
    }
}

#[cfg(unix)]
async fn reply(
    wr: &mut (impl tokio::io::AsyncWrite + Unpin),
    id: u64,
    answer: Result<Option<RespData>, String>,
) -> Result<()> {
    send(wr, reply_frame(id, answer)).await
}

#[cfg(unix)]
async fn send(wr: &mut (impl tokio::io::AsyncWrite + Unpin), msg: FromDaemon) -> Result<()> {
    wr.write_all(encode(&msg).as_bytes())
        .await
        .context("failed to write to the gui")
}

/// The peer's effective uid, or `None` when the socket layer will not say.
#[cfg(unix)]
fn peer_euid(stream: &UnixStream) -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;

        const XUCRED_VERSION: u32 = 0;
        let mut cred: libc::xucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::xucred>() as u32;
        // SAFETY: a valid fd and a correctly sized buffer
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERCRED,
                &mut cred as *mut libc::xucred as *mut libc::c_void,
                &mut len,
            )
        };
        if rc == 0
            && cred.cr_version == XUCRED_VERSION
            && len as usize >= std::mem::size_of::<libc::xucred>()
        {
            Some(cred.cr_uid)
        } else {
            None
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = stream;
        None
    }
}

// the peer check that lets the test connect exists on macOS only
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use corplink_ipc::{LoginPrompt, RunState};
    use std::time::Duration;
    use tokio::io::BufReader;

    const READ_TIMEOUT: Duration = Duration::from_secs(5);

    struct TestDaemon {
        shared: Arc<DaemonShared>,
        cmd_rx: mpsc::Receiver<Command>,
        path: PathBuf,
        logs: Arc<LogBuffer>,
        bridge: Arc<LoginBridge>,
    }

    impl TestDaemon {
        async fn start(allowed_uid: u32) -> TestDaemon {
            // nanos alone can collide between tests starting in the same
            // nanosecond; the counter makes every path unique
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("corplink-ipc-test-{unique}-{seq}.sock"));
            let logs = Arc::new(LogBuffer::new());
            let bridge = Arc::new(LoginBridge::new());
            let (status_tx, _) = watch::channel(Status {
                state: RunState::Idle,
                version: "test".into(),
                ..Status::default()
            });
            let (cmd_tx, cmd_rx) = mpsc::channel(4);
            let shared = Arc::new(DaemonShared {
                status: status_tx,
                logs: logs.clone(),
                bridge: bridge.clone(),
                cmd_tx,
                allowed_uid,
            });
            serve_at(&path, shared.clone()).await.unwrap();
            TestDaemon {
                shared,
                cmd_rx,
                path,
                logs,
                bridge,
            }
        }
    }

    impl Drop for TestDaemon {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// A connected GUI: its reader and writer halves.
    struct Gui {
        lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        wr: tokio::net::unix::OwnedWriteHalf,
    }

    impl Gui {
        async fn connect(path: &Path) -> Gui {
            let stream = UnixStream::connect(path).await.unwrap();
            let (rd, wr) = stream.into_split();
            Gui {
                lines: BufReader::new(rd).lines(),
                wr,
            }
        }

        async fn send(&mut self, msg: &ToDaemon) {
            self.wr.write_all(encode(msg).as_bytes()).await.unwrap();
            self.wr.flush().await.unwrap();
        }

        async fn recv(&mut self) -> FromDaemon {
            let line = tokio::time::timeout(READ_TIMEOUT, self.lines.next_line())
                .await
                .expect("timed out waiting for a frame")
                .unwrap()
                .expect("the daemon closed the connection");
            decode(&line).unwrap()
        }

        /// The next frame, asserting it is a reply to `id`.
        async fn recv_reply(&mut self, id: u64) -> FromDaemon {
            loop {
                match self.recv().await {
                    frame @ FromDaemon::Reply { .. } => {
                        assert_eq!(frame_reply_id(&frame), id);
                        return frame;
                    }
                    _ => continue, // skip interleaved events
                }
            }
        }
    }

    fn frame_reply_id(frame: &FromDaemon) -> u64 {
        match frame {
            FromDaemon::Reply { id, .. } => *id,
            other => panic!("not a reply: {other:?}"),
        }
    }

    async fn connect_accepted(allowed_uid: u32) -> (TestDaemon, Gui) {
        let daemon = TestDaemon::start(allowed_uid).await;
        let gui = Gui::connect(&daemon.path).await;
        (daemon, gui)
    }

    /// Consumes the two initial frames every fresh connection receives.
    async fn drain_initial_frames(gui: &mut Gui) {
        assert!(matches!(gui.recv().await, FromDaemon::StatusChanged { .. }));
        assert!(matches!(
            gui.recv().await,
            FromDaemon::LoginPromptUpdate { .. }
        ));
    }

    #[tokio::test]
    async fn a_fresh_gui_gets_status_and_can_query_it() {
        let (daemon, mut gui) = connect_accepted(unsafe { libc::getuid() }).await;

        // the initial events arrive without asking
        match gui.recv().await {
            FromDaemon::StatusChanged { status } => {
                assert_eq!(status.state, RunState::Idle);
                assert_eq!(status.version, "test");
            }
            other => panic!("expected the initial status, got {other:?}"),
        }
        assert!(matches!(
            gui.recv().await,
            FromDaemon::LoginPromptUpdate { prompt: None }
        ));

        gui.send(&ToDaemon::Status { id: 7 }).await;
        match gui.recv_reply(7).await {
            FromDaemon::Reply {
                ok: true,
                data: Some(RespData::Status { status }),
                ..
            } => {
                assert_eq!(status.state, RunState::Idle);
            }
            other => panic!("expected a status reply, got {other:?}"),
        }

        // a status update is pushed to the connected gui
        daemon.shared.update_status(Status {
            state: RunState::Online,
            server: Some("Node03".into()),
            ..daemon.shared.current_status()
        });
        match gui.recv().await {
            FromDaemon::StatusChanged { status } => {
                assert_eq!(status.state, RunState::Online);
                assert_eq!(status.server.as_deref(), Some("Node03"));
            }
            other => panic!("expected the pushed status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn logs_return_the_tail_then_stream_live_lines() {
        let (daemon, mut gui) = connect_accepted(unsafe { libc::getuid() }).await;
        daemon.logs.push("[info] old one".to_string());
        daemon.logs.push("[info] old two".to_string());

        gui.send(&ToDaemon::Logs { id: 3, tail: 1 }).await;
        match gui.recv_reply(3).await {
            FromDaemon::Reply {
                ok: true,
                data: Some(RespData::Logs { lines }),
                ..
            } => assert_eq!(lines, vec!["[info] old two".to_string()]),
            other => panic!("expected a logs reply, got {other:?}"),
        }

        daemon.logs.push("[info] live one".to_string());
        match gui.recv().await {
            FromDaemon::LogLine { line } => assert_eq!(line, "[info] live one"),
            other => panic!("expected a live log line, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn login_prompts_reach_the_gui_and_answers_flow_back() {
        let (daemon, mut gui) = connect_accepted(unsafe { libc::getuid() }).await;
        // wait until the connection is fully up: gui_attach has run, so the
        // bridge considers itself attended and prompts can stand
        drain_initial_frames(&mut gui).await;
        let bridge = daemon.bridge.clone();
        let prompt_task = tokio::spawn(async move {
            bridge
                .prompt(LoginPrompt::Qr {
                    url: "https://auth.example/xyz".into(),
                })
                .await
                .unwrap()
        });

        match gui.recv().await {
            FromDaemon::LoginPromptUpdate {
                prompt: Some(LoginPrompt::Qr { url }),
            } => assert_eq!(url, "https://auth.example/xyz"),
            other => panic!("expected the QR prompt, got {other:?}"),
        }

        // answering without a matching... the prompt stands, so the answer flows
        gui.send(&ToDaemon::LoginReply {
            id: 1,
            value: "scanned".into(),
        })
        .await;
        match gui.recv_reply(1).await {
            FromDaemon::Reply { ok: true, .. } => {}
            other => panic!("expected an ok reply, got {other:?}"),
        }
        assert_eq!(prompt_task.await.unwrap(), "scanned");

        // the cleared prompt is pushed too
        match gui.recv().await {
            FromDaemon::LoginPromptUpdate { prompt: None } => {}
            other => panic!("expected the cleared prompt, got {other:?}"),
        }

        // with no prompt standing, replies are rejected
        gui.send(&ToDaemon::LoginReply {
            id: 2,
            value: "nobody asked".into(),
        })
        .await;
        match gui.recv_reply(2).await {
            FromDaemon::Reply {
                ok: false, error, ..
            } => {
                assert!(error.unwrap().contains("no login prompt"))
            }
            other => panic!("expected an error reply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_commands_reach_the_supervisor_and_answers_flow_back() {
        let (mut daemon, mut gui) = connect_accepted(unsafe { libc::getuid() }).await;

        gui.send(&ToDaemon::Connect {
            id: 10,
            server: Some("Node05".into()),
        })
        .await;
        match daemon.cmd_rx.recv().await.unwrap() {
            Command::Connect { server, reply } => {
                assert_eq!(server.as_deref(), Some("Node05"));
                reply.send(Ok(())).unwrap();
            }
            other => panic!("expected a connect command, got {other:?}"),
        }
        match gui.recv_reply(10).await {
            FromDaemon::Reply { ok: true, .. } => {}
            other => panic!("expected an ok reply, got {other:?}"),
        }

        gui.send(&ToDaemon::Disconnect { id: 11 }).await;
        match daemon.cmd_rx.recv().await.unwrap() {
            Command::Disconnect { reply } => {
                reply.send(Err("already idle".into())).unwrap();
            }
            other => panic!("expected a disconnect command, got {other:?}"),
        }
        match gui.recv_reply(11).await {
            FromDaemon::Reply {
                ok: false, error, ..
            } => {
                assert_eq!(error.unwrap(), "already idle")
            }
            other => panic!("expected an error reply, got {other:?}"),
        }

        gui.send(&ToDaemon::Servers { id: 12 }).await;
        match daemon.cmd_rx.recv().await.unwrap() {
            Command::Servers { reply } => {
                reply
                    .send(Ok(vec![VpnServer {
                        name: "节点".into(),
                        en_name: "Node03".into(),
                        protocol: "tcp".into(),
                        latency_ms: Some(31),
                    }]))
                    .unwrap();
            }
            other => panic!("expected a servers command, got {other:?}"),
        }
        match gui.recv_reply(12).await {
            FromDaemon::Reply {
                ok: true,
                data: Some(RespData::Servers { servers }),
                ..
            } => assert_eq!(servers.len(), 1),
            other => panic!("expected a servers reply, got {other:?}"),
        }

        gui.send(&ToDaemon::Restart { id: 13 }).await;
        match daemon.cmd_rx.recv().await.unwrap() {
            Command::Restart { reply } => {
                reply.send(Err("not started by launchd".into())).unwrap();
            }
            other => panic!("expected a restart command, got {other:?}"),
        }
        match gui.recv_reply(13).await {
            FromDaemon::Reply {
                ok: false, error, ..
            } => {
                assert_eq!(error.unwrap(), "not started by launchd")
            }
            other => panic!("expected an error reply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_slow_request_holds_up_neither_events_nor_later_requests() {
        let (mut daemon, mut gui) = connect_accepted(unsafe { libc::getuid() }).await;
        drain_initial_frames(&mut gui).await;

        // a server probe takes seconds to answer
        gui.send(&ToDaemon::Servers { id: 20 }).await;
        let probe = match daemon.cmd_rx.recv().await.unwrap() {
            Command::Servers { reply } => reply,
            other => panic!("expected a servers command, got {other:?}"),
        };

        // meanwhile requests are still answered and events still pushed
        gui.send(&ToDaemon::Status { id: 21 }).await;
        assert_eq!(frame_reply_id(&gui.recv().await), 21);
        daemon.shared.update_status(Status {
            state: RunState::Connecting,
            ..daemon.shared.current_status()
        });
        assert!(matches!(
            gui.recv().await,
            FromDaemon::StatusChanged { status } if status.state == RunState::Connecting
        ));

        probe.send(Ok(Vec::new())).unwrap();
        match gui.recv().await {
            FromDaemon::Reply {
                id: 20,
                ok: true,
                data: Some(RespData::Servers { servers }),
                ..
            } => assert!(servers.is_empty()),
            other => panic!("expected the servers reply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn foreign_uids_are_refused() {
        // allow root and root+1000 (nobody relevant); we connect as ourselves
        let daemon = TestDaemon::start(unsafe { libc::getuid() } + 1000).await;
        let mut gui = Gui::connect(&daemon.path).await;
        // the connection is closed without any initial frame
        let line = tokio::time::timeout(READ_TIMEOUT, gui.lines.next_line())
            .await
            .unwrap()
            .unwrap();
        assert!(line.is_none(), "expected the connection to be closed");
    }
}
