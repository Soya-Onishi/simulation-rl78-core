//! Node log pipeline: each runtime thread sends its `log` records over mpsc to
//! the log thread, which writes them to the stream toward the Orchestrator host.
//!
//! The sender is thread-local, so several runtimes in one process (tests) keep
//! their records apart. Records from threads without a sender are dropped.

use std::cell::RefCell;
use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, SyncSender};

use contract::{NodeLogLevel, NodeLogRecord};

/// Records beyond this many in flight are dropped so logging never blocks a thread.
pub(crate) const LOG_QUEUE_CAP: usize = 256;

thread_local! {
    static THREAD_SENDER: RefCell<Option<SyncSender<NodeLogRecord>>> = const { RefCell::new(None) };
}

pub(crate) fn channel() -> (SyncSender<NodeLogRecord>, Receiver<NodeLogRecord>) {
    mpsc::sync_channel(LOG_QUEUE_CAP)
}

/// Send this thread's `log` records to `sender` until the thread exits.
pub(crate) fn attach(sender: SyncSender<NodeLogRecord>) {
    THREAD_SENDER.with(|slot| *slot.borrow_mut() = Some(sender));
}

/// Install the process-wide `log` front end unless another logger is installed.
pub(crate) fn install() {
    if log::set_logger(&NodeLogger).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

struct NodeLogger;

impl log::Log for NodeLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // `try_with`: a thread-local destructor may log after the slot is gone.
        let _ = THREAD_SENDER.try_with(|slot| {
            if let Some(sender) = &*slot.borrow() {
                // A full queue or an exited log thread drops the record.
                let _ = sender.try_send(NodeLogRecord {
                    level: level(record.level()),
                    target: record.target().into(),
                    message: record.args().to_string(),
                });
            }
        });
    }

    fn flush(&self) {}
}

fn level(level: log::Level) -> NodeLogLevel {
    match level {
        log::Level::Error => NodeLogLevel::Error,
        log::Level::Warn => NodeLogLevel::Warn,
        log::Level::Info => NodeLogLevel::Info,
        log::Level::Debug => NodeLogLevel::Debug,
        log::Level::Trace => NodeLogLevel::Trace,
    }
}

/// Write each record to `stream` until every sender is dropped.
///
/// After the first write failure the error goes to stderr once and later
/// records are drained and dropped, so the Node keeps running.
pub(crate) fn log_loop(records: &Receiver<NodeLogRecord>, mut stream: impl Write) {
    let mut broken = false;
    for record in records {
        if broken {
            continue;
        }
        if let Err(e) = stream
            .write_all(&record.to_line())
            .and_then(|()| stream.flush())
        {
            eprintln!("sim-node: log stream write failed; dropping further logs: {e}");
            broken = true;
        }
    }
}

/// Stand-in for the TCP stream to the Orchestrator host: discards every byte.
pub(crate) struct DummyTcpStream;

impl Write for DummyTcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use log::Log;

    use super::*;

    fn emit(level: log::Level, message: &str) {
        NodeLogger.log(
            &log::Record::builder()
                .level(level)
                .target("test")
                .args(format_args!("{message}"))
                .build(),
        );
    }

    fn record(message: &str) -> NodeLogRecord {
        NodeLogRecord {
            level: NodeLogLevel::Info,
            target: "test".into(),
            message: message.into(),
        }
    }

    #[test]
    fn attached_thread_sends_records_to_its_channel() {
        let (tx, rx) = channel();
        thread::spawn(move || {
            attach(tx);
            emit(log::Level::Warn, "first");
            emit(log::Level::Debug, "filtered");
        })
        .join()
        .unwrap();
        let records: Vec<_> = rx.iter().collect();
        assert_eq!(
            records,
            [NodeLogRecord {
                level: NodeLogLevel::Warn,
                target: "test".into(),
                message: "first".into(),
            }]
        );
    }

    #[test]
    fn unattached_thread_logging_is_a_no_op() {
        thread::spawn(|| emit(log::Level::Error, "nobody listens"))
            .join()
            .unwrap();
    }

    #[test]
    fn full_queue_drops_records_without_blocking() {
        let (tx, rx) = channel();
        thread::spawn(move || {
            attach(tx);
            for i in 0..=LOG_QUEUE_CAP {
                emit(log::Level::Info, &i.to_string());
            }
        })
        .join()
        .unwrap();
        assert_eq!(rx.iter().count(), LOG_QUEUE_CAP);
    }

    #[test]
    fn log_loop_writes_one_line_per_record_until_disconnect() {
        let (tx, rx) = channel();
        tx.send(record("a")).unwrap();
        tx.send(record("b")).unwrap();
        drop(tx);
        let mut out = Vec::new();
        log_loop(&rx, &mut out);
        let lines: Vec<_> = out
            .split_inclusive(|&b| b == b'\n')
            .map(|line| NodeLogRecord::from_line(line).unwrap())
            .collect();
        assert_eq!(lines, [record("a"), record("b")]);
    }

    struct FailingStream {
        writes: usize,
    }

    impl Write for FailingStream {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "peer closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn log_loop_stops_writing_after_first_failure_but_drains() {
        let (tx, rx) = channel();
        for message in ["a", "b", "c"] {
            tx.send(record(message)).unwrap();
        }
        drop(tx);
        let mut stream = FailingStream { writes: 0 };
        log_loop(&rx, &mut stream);
        assert_eq!(stream.writes, 1);
        assert!(rx.try_recv().is_err());
    }
}
