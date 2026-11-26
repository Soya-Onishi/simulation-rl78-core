//! Named thread group shared by the Command and Node runtimes.
//!
//! Every thread raises the group's shutdown flag when it exits, including by
//! panic, so one failing thread stops the rest.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

// A panicking thread must unwind so its `ShutdownOnExit` guard stops the rest.
#[cfg(not(panic = "unwind"))]
compile_error!("orchestration runtimes require panic = \"unwind\"");

/// A thread of a [`ThreadGroup`] panicked (the payload is the thread name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadPanicked(pub String);

impl fmt::Display for ThreadPanicked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "thread {} panicked", self.0)
    }
}

impl std::error::Error for ThreadPanicked {}

/// Threads that stop together. Dropping raises the flag and joins them.
pub struct ThreadGroup<E> {
    shutdown: Arc<AtomicBool>,
    threads: Vec<(String, JoinHandle<Result<(), E>>)>,
}

impl<E: Send + 'static> Default for ThreadGroup<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: Send + 'static> ThreadGroup<E> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            shutdown: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        }
    }

    /// The flag raised on [`Self::shutdown`] or when any thread exits.
    #[must_use]
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    /// `false` once shutdown was requested or any thread has exited.
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.shutdown.load(Ordering::Acquire)
    }

    pub fn spawn(
        &mut self,
        name: &str,
        body: impl FnOnce() -> Result<(), E> + Send + 'static,
    ) -> io::Result<()> {
        let on_exit = ShutdownOnExit(self.shutdown.clone());
        let handle = thread::Builder::new().name(name.into()).spawn(move || {
            let _on_exit = on_exit;
            body()
        })?;
        self.threads.push((name.into(), handle));
        Ok(())
    }

    /// Raise the flag, join every thread, and return the first error in spawn order.
    pub fn shutdown(&mut self) -> Result<(), E>
    where
        E: From<ThreadPanicked>,
    {
        self.shutdown.store(true, Ordering::Release);
        let mut first = Ok(());
        for (name, handle) in self.threads.drain(..) {
            let result = handle
                .join()
                .unwrap_or_else(|_| Err(ThreadPanicked(name).into()));
            if first.is_ok() {
                first = result;
            }
        }
        first
    }
}

impl<E> Drop for ThreadGroup<E> {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        for (_, handle) in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Raises the shutdown flag when a thread exits, including by panic.
struct ShutdownOnExit(Arc<AtomicBool>);

impl Drop for ShutdownOnExit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn wait_until_stopped<E: Send + 'static>(group: &ThreadGroup<E>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while group.is_running() {
            assert!(Instant::now() < deadline, "thread group kept running");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn panicking_thread_stops_group_and_reports_panic() {
        let mut group = ThreadGroup::<ThreadPanicked>::new();
        let flag = group.shutdown_flag();
        group
            .spawn("waits-for-shutdown", move || {
                while !flag.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            })
            .unwrap();
        group
            .spawn("panics", || panic!("simulated thread failure"))
            .unwrap();

        wait_until_stopped(&group);
        assert_eq!(group.shutdown(), Err(ThreadPanicked("panics".into())));
    }

    #[test]
    fn returning_thread_stops_group() {
        let mut group = ThreadGroup::<ThreadPanicked>::new();
        group.spawn("returns", || Ok(())).unwrap();
        wait_until_stopped(&group);
        assert_eq!(group.shutdown(), Ok(()));
    }

    #[test]
    fn first_error_in_spawn_order_is_reported() {
        let mut group = ThreadGroup::<ThreadPanicked>::new();
        group
            .spawn("first", || Err(ThreadPanicked("first error".into())))
            .unwrap();
        group
            .spawn("second", || Err(ThreadPanicked("second error".into())))
            .unwrap();
        assert_eq!(group.shutdown(), Err(ThreadPanicked("first error".into())));
    }
}
