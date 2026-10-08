//! The daemon connection, on its own thread: plain std sockets, one JSON
//! line per frame. Reconnects forever; the UI sees connection health through
//! the events it feeds it.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::Duration;

use corplink_ipc::{decode, encode, socket_candidates, FromDaemon, ToDaemon};

/// What the ipc thread tells the UI thread. The frame is boxed: the other
/// variants are empty and an unboxed `FromDaemon` would pad every message.
pub enum AppEvent {
    Connected,
    Disconnected,
    Frame(Box<FromDaemon>),
}

/// What the UI thread asks the ipc thread to send.
pub enum Outbound {
    Send(ToDaemon),
    /// stop the thread (app exit)
    Quit,
}

const RECONNECT_WAIT: Duration = Duration::from_secs(1);
/// How often the pump thread checks whether the reader thread is still alive.
const POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Spawns the ipc thread. The returned sender feeds outbound requests; inbound
/// frames arrive on `events`.
pub fn spawn(events: Sender<AppEvent>) -> Sender<Outbound> {
    let (tx, rx) = std::sync::mpsc::channel::<Outbound>();
    std::thread::Builder::new()
        .name("ipc-client".into())
        .spawn(move || run(events, rx))
        .expect("failed to spawn the ipc thread");
    tx
}

fn run(events: Sender<AppEvent>, outbound: Receiver<Outbound>) {
    loop {
        let stream = connect_forever();
        // requests made while no daemon answered were meant for what the UI
        // showed back then: a connect clicked into a dead socket must not
        // restart the session of a daemon that came up since
        loop {
            match outbound.try_recv() {
                Ok(Outbound::Send(_)) => continue,
                Ok(Outbound::Quit) | Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => break,
            }
        }
        let Ok(cloned) = stream.try_clone() else {
            std::thread::sleep(RECONNECT_WAIT);
            continue;
        };
        let mut writer = BufWriterShim { inner: stream };
        let _ = events.send(AppEvent::Connected);

        let reader = std::thread::Builder::new()
            .name("ipc-reader".into())
            .spawn({
                let events = events.clone();
                let reader = BufReader::new(cloned);
                move || read_loop(reader, &events)
            })
            .expect("failed to spawn the reader thread");

        // pump requests until the reader dies (its exit means the connection
        // is gone) or a Quit arrives. recv_timeout, not recv: a dead reader
        // with no outbound traffic would otherwise block the reconnect
        // forever.
        loop {
            match outbound.recv_timeout(POLL_INTERVAL) {
                Ok(Outbound::Send(msg)) => {
                    if writer.write_line(&encode(&msg)).is_err() {
                        break;
                    }
                }
                Ok(Outbound::Quit) => {
                    let _ = events.send(AppEvent::Disconnected);
                    return;
                }
                // the UI went away
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {
                    if reader.is_finished() {
                        break;
                    }
                }
            }
        }
        // a failed write can leave the read side open; shutting both down
        // ends the reader thread, which is joined next
        let _ = writer.inner.shutdown(Shutdown::Both);
        let _ = reader.join();
        let _ = events.send(AppEvent::Disconnected);
        std::thread::sleep(RECONNECT_WAIT);
    }
}

/// Tries every candidate path until one accepts, sleeping between rounds.
fn connect_forever() -> UnixStream {
    loop {
        for path in socket_candidates() {
            if let Ok(stream) = UnixStream::connect(&path) {
                return stream;
            }
        }
        std::thread::sleep(RECONNECT_WAIT);
    }
}

fn read_loop(mut reader: BufReader<UnixStream>, events: &Sender<AppEvent>) {
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return, // daemon closed the connection
            Ok(_) => {
                if let Some(frame) = decode_line(&line) {
                    if events.send(AppEvent::Frame(Box::new(frame))).is_err() {
                        return; // the UI went away
                    }
                }
            }
            Err(_) => return,
        }
    }
}

fn decode_line(line: &str) -> Option<FromDaemon> {
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    match decode(trimmed) {
        Ok(frame) => Some(frame),
        Err(e) => {
            // one bad line must not kill the stream
            eprintln!("[gui] unparsable daemon frame: {e}");
            None
        }
    }
}

struct BufWriterShim {
    inner: UnixStream,
}

impl BufWriterShim {
    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        self.inner.write_all(line.as_bytes())?;
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use corplink_ipc::RunState;

    #[test]
    fn frames_decode_and_bad_lines_are_skipped() {
        let good = corplink_ipc::encode(&FromDaemon::StatusChanged {
            status: corplink_ipc::Status {
                state: RunState::Online,
                ..Default::default()
            },
        });
        assert!(matches!(
            decode_line(&good),
            Some(FromDaemon::StatusChanged { .. })
        ));
        assert!(decode_line("\n").is_none());
        assert!(decode_line("").is_none());
        assert!(decode_line("not json").is_none());
    }
}
