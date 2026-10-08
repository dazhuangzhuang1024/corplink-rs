//! A `log::Log` wrapper that keeps the last formatted log lines in a ring
//! buffer and forwards everything to the real env_logger, so the GUI can show
//! daemon logs over IPC (the files under /var/log are root-only) while the
//! launchd log files keep receiving exactly what they received before.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::sync::broadcast;

/// Lines kept for the GUI's `logs` request.
const RING_CAPACITY: usize = 4096;
/// Live lines buffered for slow subscribers before the oldest are dropped.
const LIVE_CAPACITY: usize = 1024;

pub struct LogBuffer {
    ring: Mutex<VecDeque<String>>,
    live: broadcast::Sender<String>,
}

impl LogBuffer {
    pub(crate) fn new() -> LogBuffer {
        let (live, _) = broadcast::channel(LIVE_CAPACITY);
        LogBuffer {
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
            live,
        }
    }

    /// The last `n` buffered lines, oldest first.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let ring = self.ring.lock().unwrap();
        let skip = ring.len().saturating_sub(n);
        ring.iter().skip(skip).cloned().collect()
    }

    /// New lines as they are logged. A line is delivered to every subscriber
    /// that keeps up; slow ones miss lines (the ring still has them).
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.live.subscribe()
    }

    pub(crate) fn push(&self, line: String) {
        {
            let mut ring = self.ring.lock().unwrap();
            if ring.len() == RING_CAPACITY {
                ring.pop_front();
            }
            ring.push_back(line.clone());
        }
        // no subscribers or a lagging one is not an error here
        let _ = self.live.send(line);
    }
}

struct ForwardingLogger {
    inner: env_logger::Logger,
    buffer: Arc<LogBuffer>,
}

impl log::Log for ForwardingLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        // only mirror lines that env_logger writes, so the GUI sees the same
        // log the file under /var/log has
        if self.inner.matches(record) {
            self.buffer.push(format_line(record));
            self.inner.log(record);
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// env_logger's default line shape, e.g.
/// `[2026-10-07T16:45:52Z INFO  corplink_rs::dns] split dns: removed ...`
fn format_line(record: &log::Record) -> String {
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    format!("[{} {:<5} {}] {}", ts, record.level(), record.target(), record.args())
}

/// Installs the wrapper around the env_logger built from the environment.
/// Returns the buffer the IPC server serves `logs` requests from.
pub fn init() -> Result<Arc<LogBuffer>> {
    let inner = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .build();
    // the ceiling env_logger itself would set: disabled levels stay a cheap
    // static check at every log call site, in every crate
    let max_level = inner.filter();
    let buffer = Arc::new(LogBuffer::new());
    log::set_boxed_logger(Box::new(ForwardingLogger {
        inner,
        buffer: buffer.clone(),
    }))
    .context("failed to install the log buffer")?;
    log::set_max_level(max_level);
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn log_lines(lines: &[&str]) -> Arc<LogBuffer> {
        let buffer = Arc::new(LogBuffer::new());
        for line in lines {
            buffer.push(line.to_string());
        }
        buffer
    }

    #[test]
    fn tail_returns_the_last_n_lines_oldest_first() {
        let buffer = log_lines(&["a", "b", "c", "d"]);
        assert_eq!(buffer.tail(2), vec!["c", "d"]);
        assert_eq!(buffer.tail(10), vec!["a", "b", "c", "d"]);
        assert!(buffer.tail(0).is_empty());
    }

    #[test]
    fn ring_evicts_the_oldest_beyond_capacity() {
        let buffer = log_lines(&[]);
        for i in 0..(RING_CAPACITY + 10) {
            buffer.push(format!("line {i}"));
        }
        let tail = buffer.tail(3);
        assert_eq!(
            tail,
            vec![
                format!("line {}", RING_CAPACITY + 7),
                format!("line {}", RING_CAPACITY + 8),
                format!("line {}", RING_CAPACITY + 9),
            ]
        );
    }

    #[tokio::test]
    async fn subscribers_receive_pushed_lines() {
        let buffer = log_lines(&["old"]);
        let mut rx = buffer.subscribe();
        buffer.push("new".to_string());
        // subscribe() happened after "old", so only "new" may arrive
        assert_eq!(rx.recv().await.unwrap(), "new");
    }

    #[tokio::test]
    async fn a_line_without_subscribers_is_not_an_error() {
        let buffer = log_lines(&[]);
        buffer.push("nobody listens".to_string());
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(buffer.tail(1), vec!["nobody listens"]);
    }

    #[test]
    fn format_line_matches_env_logger_shape() {
        let record = log::Record::builder()
            .level(log::Level::Warn)
            .target("corplink_rs::dns")
            .args(format_args!("split dns: removed /etc/resolver/corp.example"))
            .build();
        let line = format_line(&record);
        assert!(line.starts_with('['));
        assert!(line.contains("WARN  corplink_rs::dns] split dns: removed"));
    }
}
