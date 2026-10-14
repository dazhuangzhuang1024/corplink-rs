//! The daemon's interactive login surface.
//!
//! Three points in the login/connect flow need a human: confirming a scanned
//! QR code, entering an emailed login code, and entering a 2FA code. A
//! foreground run serves them from the terminal exactly as before; the daemon
//! instead serves them from the GUI over IPC, via [`LoginBridge`]: the prompt
//! is published to every connected GUI, the answer comes back through the
//! bridge, and without a GUI in sight the prompt fails the login immediately
//! instead of blocking forever (preserving the unattended launchd behavior).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use corplink_ipc::LoginPrompt;
use tokio::sync::{oneshot, watch, Notify};

use crate::qrcode::TerminalQrCode;
use crate::utils;

/// How long a published prompt stays answerable. Generous: scanning a QR code
/// or fetching an email code can take minutes, and the daemon has nothing
/// better to do while it waits.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, PartialEq)]
pub enum PromptError {
    Cancelled,
    NoUi,
    TimedOut,
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptError::Cancelled => write!(f, "cancelled"),
            PromptError::NoUi => write!(
                f,
                "no interactive UI available: no GUI has connected and stdin is not usable \
                 (e.g. running as a service); log in from corplink-gui, or stop the service \
                 and run corplink-rs once in a terminal"
            ),
            PromptError::TimedOut => write!(f, "no answer within {}s", PROMPT_TIMEOUT.as_secs()),
        }
    }
}

impl std::error::Error for PromptError {}

/// The rendezvous between the login flow and any connected GUIs.
///
/// At most one prompt is active at a time (a single session connects at a
/// time); it is published through a `watch` channel so a GUI that connects
/// late still finds it, and answered through a oneshot slot.
pub struct LoginBridge {
    prompt_tx: watch::Sender<Option<LoginPrompt>>,
    // the answer slot of the published prompt; the prompt is published and
    // withdrawn under this lock too, so the two never disagree
    slot: Mutex<Slot>,
    prompt_timeout: Duration,
    gui_connections: AtomicUsize,
    ever_had_gui: AtomicBool,
    // notified when the first GUI connection ever appears
    gui_arrived: Notify,
}

#[derive(Default)]
struct Slot {
    // bumped by every prompt and cancel. A prompt only withdraws what it
    // published itself: a superseded or cancelled one must not clobber
    // whoever took its place.
    gen: u64,
    answer: Option<oneshot::Sender<String>>,
}

/// Withdraws a published prompt however its wait ends, also when the login
/// task is aborted mid-wait and the code after the await never runs.
struct Withdraw<'a> {
    bridge: &'a LoginBridge,
    gen: u64,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        let mut slot = self.bridge.slot();
        if slot.gen == self.gen {
            slot.answer = None;
            self.bridge.prompt_tx.send_replace(None);
        }
    }
}

impl Default for LoginBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginBridge {
    pub fn new() -> LoginBridge {
        LoginBridge::with_prompt_timeout(PROMPT_TIMEOUT)
    }

    fn with_prompt_timeout(prompt_timeout: Duration) -> LoginBridge {
        let (prompt_tx, _) = watch::channel(None);
        LoginBridge {
            prompt_tx,
            slot: Mutex::new(Slot::default()),
            prompt_timeout,
            gui_connections: AtomicUsize::new(0),
            ever_had_gui: AtomicBool::new(false),
            gui_arrived: Notify::new(),
        }
    }

    // never held across an await, and nothing panics while holding it
    fn slot(&self) -> MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current prompt, `None` when none is active. The watch channel
    /// replays it to GUIs that connect while it is up.
    pub fn subscribe_prompts(&self) -> watch::Receiver<Option<LoginPrompt>> {
        self.prompt_tx.subscribe()
    }

    /// Whether a GUI has ever been connected. Once true the daemon considers
    /// itself attended: prompts keep waiting for input instead of failing.
    pub fn ever_had_gui(&self) -> bool {
        self.ever_had_gui.load(Ordering::SeqCst)
    }

    pub fn gui_attach(&self) {
        if self.gui_connections.fetch_add(1, Ordering::SeqCst) == 0 {
            self.ever_had_gui.store(true, Ordering::SeqCst);
            // notify_one (not notify_waiters): the stored permit also wakes a
            // waiter that registers only after this call
            self.gui_arrived.notify_one();
        }
    }

    /// Wait until a GUI has ever connected, at most `timeout`. Used by the
    /// supervisor to give a GUI that is attaching right now a chance before
    /// treating a failure as unattended.
    pub async fn wait_for_gui(&self, timeout: Duration) -> bool {
        if self.ever_had_gui() {
            return true;
        }
        tokio::time::timeout(timeout, async {
            loop {
                if self.ever_had_gui() {
                    return;
                }
                self.gui_arrived.notified().await;
            }
        })
        .await
        .is_ok()
    }

    pub fn gui_detach(&self) {
        // connections only ever detach after attaching
        debug_assert!(self.gui_connections.load(Ordering::SeqCst) > 0);
        self.gui_connections.fetch_sub(1, Ordering::SeqCst);
    }

    /// Publish `prompt` and wait for the answer. Fails without waiting when no
    /// GUI has ever connected (the unattended case); once one has, later
    /// prompts wait out the full timeout even between GUI connections, since
    /// the user may simply reopen the app.
    pub async fn prompt(&self, prompt: LoginPrompt) -> Result<String, PromptError> {
        if !self.ever_had_gui() {
            return Err(PromptError::NoUi);
        }
        let (tx, rx) = oneshot::channel();
        let gen = {
            let mut slot = self.slot();
            slot.gen += 1;
            // replacing a still-standing sender fails that waiter with
            // Cancelled
            slot.answer = Some(tx);
            self.prompt_tx.send_replace(Some(prompt));
            slot.gen
        };
        let _withdraw = Withdraw { bridge: self, gen };
        match tokio::time::timeout(self.prompt_timeout, rx).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(_)) => Err(PromptError::Cancelled),
            Err(_) => Err(PromptError::TimedOut),
        }
    }

    /// Deliver `value` to the active prompt; false when none is active.
    pub fn reply(&self, value: String) -> bool {
        // the waiter withdraws the published prompt once it has the answer
        let answer = self.slot().answer.take();
        match answer {
            Some(tx) => tx.send(value).is_ok(),
            None => false,
        }
    }

    /// Abort the active prompt (GUI "cancel"); false when none is active.
    pub fn cancel(&self) -> bool {
        let mut slot = self.slot();
        slot.gen += 1;
        self.prompt_tx.send_replace(None);
        // dropping the sender fails the waiter with Cancelled
        slot.answer.take().is_some()
    }
}

/// How the three interactive prompts are served.
#[derive(Clone)]
pub enum UiMode {
    /// A foreground run: print to the terminal, read from stdin.
    Terminal,
    /// The daemon: prompt connected GUIs through the bridge.
    Ipc(Arc<LoginBridge>),
}

#[derive(Clone)]
pub struct LoginUi {
    mode: UiMode,
}

impl LoginUi {
    pub fn terminal() -> LoginUi {
        LoginUi {
            mode: UiMode::Terminal,
        }
    }

    pub fn ipc(bridge: Arc<LoginBridge>) -> LoginUi {
        LoginUi {
            mode: UiMode::Ipc(bridge),
        }
    }

    /// Fails like a prompt would when nobody could answer one, for steps that
    /// must not even start then (requesting an email code mails one).
    pub fn ensure_answerable(&self) -> Result<(), PromptError> {
        match &self.mode {
            UiMode::Terminal => Ok(()),
            UiMode::Ipc(bridge) if bridge.ever_had_gui() => Ok(()),
            UiMode::Ipc(_) => Err(PromptError::NoUi),
        }
    }

    /// Show the login QR for `url` and wait until the user confirms having
    /// scanned it (any reply value counts as the confirmation).
    pub async fn qr_confirm(&self, url: &str) -> Result<()> {
        match &self.mode {
            UiMode::Terminal => {
                log::info!(
                    "please scan the QR code or visit the following link to auth corplink:\n{url}"
                );
                match TerminalQrCode::from_bytes(url.as_bytes()) {
                    Ok(qr) => qr.print(),
                    Err(e) => log::warn!("failed to generate qr code: {e}"),
                }
                log::info!("press enter if you finish auth");
                // a plain blocking read, as this path always was: the
                // terminal user is right there
                let stdin = std::io::stdin();
                stdin.lines().next();
                Ok(())
            }
            UiMode::Ipc(bridge) => bridge
                .prompt(LoginPrompt::Qr {
                    url: url.to_string(),
                })
                .await
                .map(|_| ())
                // context, not format: the PromptError must stay in the chain
                // so the supervisor can classify the failure
                .context("the QR code login"),
        }
    }

    /// Ask for a code; `prompt` selects the kind and describes it over IPC.
    pub async fn read_code(&self, prompt: LoginPrompt) -> Result<String> {
        match &self.mode {
            UiMode::Terminal => {
                match prompt {
                    LoginPrompt::Qr { .. } => unreachable!("the QR prompt uses qr_confirm"),
                    LoginPrompt::EmailCode => log::info!("input your code from email:"),
                    LoginPrompt::TwoFa => log::info!("input your 2fa code:"),
                }
                utils::read_line().await
            }
            UiMode::Ipc(bridge) => bridge
                .prompt(prompt)
                .await
                // context, not format: see qr_confirm
                .context("the code entry"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge_with_gui() -> Arc<LoginBridge> {
        let bridge = Arc::new(LoginBridge::new());
        bridge.gui_attach();
        bridge.gui_detach();
        assert!(bridge.ever_had_gui());
        bridge
    }

    #[tokio::test]
    async fn prompts_are_answered_and_cleared() {
        let bridge = bridge_with_gui();
        let b = bridge.clone();
        let prompt_task = tokio::spawn(async move { b.prompt(LoginPrompt::EmailCode).await });

        // the prompt becomes visible, then the answer arrives
        let mut prompts = bridge.subscribe_prompts();
        assert!(prompts.changed().await.is_ok());
        assert_eq!(*prompts.borrow_and_update(), Some(LoginPrompt::EmailCode));
        assert!(bridge.reply("123456".to_string()));
        assert_eq!(prompt_task.await.unwrap().unwrap(), "123456");

        // answered prompts are cleared, and replies without a prompt are rejected
        assert_eq!(*bridge.subscribe_prompts().borrow(), None);
        assert!(!bridge.reply("late".to_string()));
    }

    #[tokio::test]
    async fn a_late_gui_sees_the_standing_prompt() {
        let bridge = Arc::new(LoginBridge::with_prompt_timeout(Duration::from_millis(80)));
        bridge.gui_attach();
        let b = bridge.clone();
        tokio::spawn(async move {
            b.prompt(LoginPrompt::TwoFa).await.unwrap_err();
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        // a GUI connecting now subscribes and finds the standing prompt
        let rx = bridge.subscribe_prompts();
        assert_eq!(*rx.borrow(), Some(LoginPrompt::TwoFa));
        assert!(bridge.cancel());
    }

    #[tokio::test]
    async fn without_ever_a_gui_prompts_fail_immediately() {
        let none = LoginBridge::new();
        assert_eq!(
            none.prompt(LoginPrompt::TwoFa).await.unwrap_err(),
            PromptError::NoUi
        );
        assert_eq!(*none.subscribe_prompts().borrow(), None);
    }

    #[tokio::test]
    async fn cancel_aborts_the_waiter() {
        let bridge = bridge_with_gui();
        let b = bridge.clone();
        let waiter =
            tokio::spawn(async move { b.prompt(LoginPrompt::Qr { url: "u".into() }).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(bridge.cancel());
        assert_eq!(waiter.await.unwrap().unwrap_err(), PromptError::Cancelled);
    }

    #[tokio::test]
    async fn an_aborted_waiter_withdraws_its_prompt() {
        // the supervisor aborts the login task; the prompt it published must
        // not outlive it, or the GUI keeps asking for an answer nobody takes
        let bridge = bridge_with_gui();
        let b = bridge.clone();
        let waiter = tokio::spawn(async move { b.prompt(LoginPrompt::EmailCode).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            *bridge.subscribe_prompts().borrow(),
            Some(LoginPrompt::EmailCode)
        );
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(*bridge.subscribe_prompts().borrow(), None);
        assert!(!bridge.reply("too late".to_string()));
    }

    #[tokio::test]
    async fn a_superseded_waiter_leaves_the_new_prompt_alone() {
        let bridge = bridge_with_gui();
        let b = bridge.clone();
        let old = tokio::spawn(async move { b.prompt(LoginPrompt::EmailCode).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        let b = bridge.clone();
        let new_task = tokio::spawn(async move { b.prompt(LoginPrompt::TwoFa).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        // the old waiter going away (after it was already superseded) keeps
        // the new prompt published and answerable
        old.abort();
        let _ = old.await;
        assert_eq!(
            *bridge.subscribe_prompts().borrow(),
            Some(LoginPrompt::TwoFa)
        );
        assert!(bridge.reply("654321".to_string()));
        assert_eq!(new_task.await.unwrap().unwrap(), "654321");
    }

    #[tokio::test]
    async fn a_new_prompt_steals_the_slot_from_the_old_one() {
        let bridge = bridge_with_gui();
        let b = bridge.clone();
        let old = tokio::spawn(async move { b.prompt(LoginPrompt::EmailCode).await });
        tokio::time::sleep(Duration::from_millis(10)).await;

        let new_task = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.prompt(LoginPrompt::TwoFa).await }
        });
        // the old waiter is dropped with its sender; the new prompt stands
        assert_eq!(old.await.unwrap().unwrap_err(), PromptError::Cancelled);
        assert_eq!(
            *bridge.subscribe_prompts().borrow(),
            Some(LoginPrompt::TwoFa)
        );
        assert!(bridge.reply("654321".to_string()));
        assert_eq!(new_task.await.unwrap().unwrap(), "654321");
    }

    #[tokio::test]
    async fn prompts_time_out() {
        let bridge = Arc::new(LoginBridge::with_prompt_timeout(Duration::from_millis(50)));
        bridge.gui_attach();
        let started = std::time::Instant::now();
        assert_eq!(
            bridge.prompt(LoginPrompt::TwoFa).await.unwrap_err(),
            PromptError::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        // a timed-out prompt is cleared, and a follow-up prompt still works
        assert_eq!(*bridge.subscribe_prompts().borrow(), None);
        let b = bridge.clone();
        let next = tokio::spawn(async move { b.prompt(LoginPrompt::EmailCode).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(bridge.reply("ok".to_string()));
        assert_eq!(next.await.unwrap().unwrap(), "ok");
    }

    #[tokio::test]
    async fn wait_for_gui_sees_attaches_within_the_window() {
        // nothing attached: times out
        let none = LoginBridge::new();
        assert!(!none.wait_for_gui(Duration::from_millis(50)).await);

        // attached before the wait: returns at once
        let early = Arc::new(LoginBridge::new());
        early.gui_attach();
        assert!(early.wait_for_gui(Duration::from_millis(50)).await);

        // attached during the wait: the stored permit wakes it
        let late = Arc::new(LoginBridge::new());
        let b = late.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            b.gui_attach();
        });
        let started = std::time::Instant::now();
        assert!(late.wait_for_gui(Duration::from_secs(5)).await);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn ui_mode_selects_the_surface() {
        // the terminal surface never consults the bridge; the ipc surface's
        // prompts fail with NoUi until a gui has ever connected
        let bridge = Arc::new(LoginBridge::with_prompt_timeout(Duration::from_millis(50)));
        let ipc_ui = LoginUi::ipc(bridge.clone());
        assert_eq!(ipc_ui.ensure_answerable(), Err(PromptError::NoUi));
        assert_eq!(LoginUi::terminal().ensure_answerable(), Ok(()));
        let err = ipc_ui.read_code(LoginPrompt::TwoFa).await.unwrap_err();
        // the context keeps the PromptError reachable in the chain
        assert!(err
            .chain()
            .any(|c| c.downcast_ref::<PromptError>() == Some(&PromptError::NoUi)));
        assert_eq!(*bridge.subscribe_prompts().borrow(), None);

        bridge.gui_attach();
        assert_eq!(ipc_ui.ensure_answerable(), Ok(()));
        let prompt = tokio::spawn({
            let ui = LoginUi::ipc(bridge.clone());
            async move { ui.read_code(LoginPrompt::TwoFa).await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            *bridge.subscribe_prompts().borrow(),
            Some(LoginPrompt::TwoFa)
        );
        assert!(bridge.reply("123456".to_string()));
        assert_eq!(prompt.await.unwrap().unwrap(), "123456");
    }
}
