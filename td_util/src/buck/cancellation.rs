/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Cancellation of the targets processes owned by one graph preparation.

use std::io;
use std::io::BufRead;
use std::io::Read;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context as _;
use thiserror::Error;

const CHILD_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Error)]
#[error("graph preparation was cancelled")]
pub struct Cancelled;

#[derive(Debug, Default)]
struct State {
    cancelled: AtomicBool,
    children: Mutex<Vec<Weak<Mutex<Child>>>>,
    wake: Condvar,
}

/// A persistent cancellation request shared by a preparation and its targets commands.
/// This never kills the Buck daemon or another preparation's command.
#[derive(Clone, Debug, Default)]
pub struct Cancellation {
    state: Arc<State>,
}

/// Checks cancellation between buffered graph-input reads.
pub struct CancellableReader<R> {
    inner: R,
    cancellation: Option<Cancellation>,
}

impl<R> CancellableReader<R> {
    pub(crate) fn new(inner: R, cancellation: Option<Cancellation>) -> Self {
        Self {
            inner,
            cancellation,
        }
    }

    fn check(&self) -> io::Result<()> {
        if let Some(cancellation) = &self.cancellation {
            cancellation.check().map_err(io::Error::other)?;
        }
        Ok(())
    }
}

impl<R: Read> Read for CancellableReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        self.inner.read(buffer)
    }
}

impl<R: BufRead> BufRead for CancellableReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.check()?;
        self.inner.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.inner.consume(amount);
    }
}

impl Cancellation {
    /// Fail before starting another phase when cancellation was requested.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.state.cancelled.load(Ordering::Acquire) {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    /// Request cancellation. The process owner must still wait to reap its child.
    pub fn cancel(&self) -> anyhow::Result<()> {
        let children = self
            .state
            .children
            .lock()
            .expect("cancellation mutex poisoned");
        self.state.cancelled.store(true, Ordering::Release);
        let mut failures = Vec::new();
        for child in children.iter().filter_map(Weak::upgrade) {
            let mut child = child.lock().expect("Buck child mutex poisoned");
            let stop = (|| -> io::Result<()> {
                if child.try_wait()?.is_some() {
                    return Ok(());
                }
                match child.kill() {
                    Ok(()) => Ok(()),
                    // Only a confirmed exit makes a failed kill harmless. In
                    // particular, EPERM by itself is not proof of termination.
                    Err(_) if child.try_wait()?.is_some() => Ok(()),
                    Err(error) => Err(error),
                }
            })();
            if let Err(error) = stop {
                failures.push(error.to_string());
            }
        }
        self.state.wake.notify_all();
        anyhow::ensure!(
            failures.is_empty(),
            "cancelling owned Buck children: {}",
            failures.join("; ")
        );
        Ok(())
    }

    pub(crate) fn spawn(&self, command: &mut Command) -> anyhow::Result<Arc<Mutex<Child>>> {
        let mut children = self
            .state
            .children
            .lock()
            .expect("cancellation mutex poisoned");
        self.check()?;
        // Registration and cancellation share the lock: cancellation cannot miss
        // a child spawned concurrently with the request.
        let child = Arc::new(Mutex::new(
            command.spawn().context("spawning cancellable Buck")?,
        ));
        children.retain(|child| child.strong_count() != 0);
        children.push(Arc::downgrade(&child));
        Ok(child)
    }

    pub(crate) fn wait(&self, child: &Mutex<Child>) -> anyhow::Result<ExitStatus> {
        let mut children = self
            .state
            .children
            .lock()
            .expect("cancellation mutex poisoned");
        loop {
            let status = child
                .lock()
                .expect("Buck child mutex poisoned")
                .try_wait()
                .context("waiting for cancellable Buck")?;
            if let Some(status) = status {
                return if self.state.cancelled.load(Ordering::Acquire) {
                    Err(Cancelled.into())
                } else {
                    Ok(status)
                };
            }
            // Never hold the child mutex across a blocking wait. The condition
            // variable makes a cancellation request wake this waiter immediately.
            children = self
                .state
                .wake
                .wait_timeout(children, CHILD_EXIT_POLL_INTERVAL)
                .expect("cancellation mutex poisoned")
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    #[test]
    fn cancellation_prevents_later_spawn() {
        let cancellation = Cancellation::default();
        cancellation.cancel().unwrap();
        assert!(cancellation.check().is_err());
        assert!(cancellation.spawn(&mut Command::new("/bin/true")).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_after_normal_exit_is_idempotent() {
        let cancellation = Cancellation::default();
        let child = cancellation
            .spawn(Command::new("/bin/sh").args(["-c", "exit 0"]))
            .unwrap();
        assert!(cancellation.wait(&child).unwrap().success());
        cancellation.cancel().unwrap();
        cancellation.cancel().unwrap();
        assert!(child.lock().unwrap().try_wait().unwrap().is_some());
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_closes_stream_and_reaps_only_owned_child() {
        let cancellation = Cancellation::default();
        let other = Cancellation::default();
        let spawn = |owner: &Cancellation| {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "printf 'ready\\n'; read value"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            let child = owner.spawn(&mut command).unwrap();
            let stdout = child.lock().unwrap().stdout.take().unwrap();
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "ready\n");
            (child, reader)
        };
        let (child, mut reader) = spawn(&cancellation);
        let (unrelated, _) = spawn(&other);
        let (done_tx, done_rx) = mpsc::channel();
        let waiter_cancel = cancellation.clone();
        let waiter_child = child.clone();
        let waiter = thread::spawn(move || {
            let mut remainder = String::new();
            reader.read_to_string(&mut remainder).unwrap();
            let error = waiter_cancel.wait(&waiter_child).unwrap_err();
            assert!(error.is::<Cancelled>());
            done_tx.send(()).unwrap();
        });
        cancellation.cancel().unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("cancelled child must release streaming reader and waiter");
        waiter.join().unwrap();
        assert!(child.lock().unwrap().try_wait().unwrap().is_some());
        cancellation.cancel().unwrap();
        assert!(unrelated.lock().unwrap().try_wait().unwrap().is_none());
        other.cancel().unwrap();
        assert!(other.wait(&unrelated).unwrap_err().is::<Cancelled>());
    }
}
