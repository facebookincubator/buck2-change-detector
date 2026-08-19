/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeMap;
use std::fmt::Display;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use serde::Serialize;
use tracing::info;

static START_TIME: OnceLock<Instant> = OnceLock::new();

pub fn init_logger_start_time() {
    START_TIME
        .set(Instant::now())
        .expect("START_TIME already initialized");
}

pub fn start_time() -> Instant {
    *START_TIME.get_or_init(Instant::now)
}

pub fn elapsed() -> Duration {
    start_time().elapsed()
}

pub fn step(name: &str) {
    info!("Starting {} at {:.3}s", name, elapsed().as_secs_f64());
}

pub fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

/// Structured logging helpers for parallel pipelines.
///
/// Lines are formatted `[<role>] <action> <name>[ (<detail>)]` where `<role>`
/// is `main` or `worker` and `<action>` is `spawn`/`start`/`done`/`join`/`info`.
/// `Phase` logs `start`/`done`; `bg_spawn`/`bg_join`/`bg_info` log the rest.
pub fn bg_spawn(name: &str) {
    info!("[main] spawn {}", name);
}

pub fn bg_join(name: &str) {
    info!("[main] join  {}", name);
}

pub fn bg_info(msg: impl Display) {
    info!("[main] info  {}", msg);
}

/// RAII guard that logs `[role] start <name>` on construction and
/// `[role] done <name> (<Yms>)` on Drop. Call [`Phase::done_with`] to log
/// immediately with extra context (counts, stats) instead of at scope exit.
#[must_use = "the Phase guard must be held for the duration of the work; \
              dropping immediately logs `done` right after `start`"]
pub struct Phase {
    name: String,
    role: &'static str,
    /// `Some` until logged; `take()` by `done_with` or `Drop` to log exactly once.
    start: Option<Instant>,
}

impl Phase {
    pub fn main(name: impl Into<String>) -> Self {
        Self::new(name.into(), "main")
    }

    pub fn worker(name: impl Into<String>) -> Self {
        Self::new(name.into(), "worker")
    }

    fn new(name: String, role: &'static str) -> Self {
        info!("[{}] start {}", role, name);
        Self {
            name,
            role,
            start: Some(Instant::now()),
        }
    }

    /// Log `done` immediately with extra context. No-op if already logged.
    pub fn done_with(&mut self, detail: impl Display) {
        if let Some(start) = self.start.take() {
            info!(
                "[{}] done  {} ({}ms; {})",
                self.role,
                self.name,
                start.elapsed().as_millis(),
                detail,
            );
        }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if let Some(start) = self.start.take() {
            info!(
                "[{}] done  {} ({}ms)",
                self.role,
                self.name,
                start.elapsed().as_millis(),
            );
        }
    }
}

/// Aggregated inclusive timing for every completed substep with the same name.
/// Concurrent or nested invocations may therefore sum to more than component
/// wall-clock time; `invocations` makes that aggregation explicit.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SubstepTiming {
    pub duration_ms: u64,
    pub invocations: u64,
}

/// Cloneable recorder for scoped substeps within one component invocation.
///
/// A substep is registered when its guard completes, so callers only need to
/// create a guard for new work; terminal telemetry can serialize [`snapshot`]
/// without maintaining a separate list of known substeps.
///
/// [`snapshot`]: SubstepRecorder::snapshot
#[derive(Clone, Debug, Default)]
pub struct SubstepRecorder {
    timings: Arc<Mutex<BTreeMap<String, SubstepTiming>>>,
}

impl SubstepRecorder {
    /// Start a main-thread substep that records and logs when its guard drops.
    pub fn substep(&self, name: impl Into<String>) -> SubstepGuard {
        SubstepGuard::new(name.into(), "main", self.clone())
    }

    /// Start a worker substep that records and logs when its guard drops.
    pub fn worker_substep(&self, name: impl Into<String>) -> SubstepGuard {
        SubstepGuard::new(name.into(), "worker", self.clone())
    }

    /// Return a stable snapshot suitable for structured telemetry.
    pub fn snapshot(&self) -> BTreeMap<String, SubstepTiming> {
        self.timings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn record(&self, name: &str, duration: Duration) {
        let mut timings = self
            .timings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let timing = timings.entry(name.to_owned()).or_default();
        timing.duration_ms = timing
            .duration_ms
            .saturating_add(duration.as_millis() as u64);
        timing.invocations = timing.invocations.saturating_add(1);
    }
}

/// RAII guard returned by [`SubstepRecorder`].
#[must_use = "the substep guard must be held for the duration of the work"]
pub struct SubstepGuard {
    name: String,
    recorder: SubstepRecorder,
    start: Option<Instant>,
    phase: Phase,
}

impl SubstepGuard {
    fn new(name: String, role: &'static str, recorder: SubstepRecorder) -> Self {
        let start = Instant::now();
        let phase = Phase::new(name.clone(), role);
        Self {
            name,
            recorder,
            start: Some(start),
            phase,
        }
    }

    /// Complete the substep immediately with extra human-readable context.
    /// No-op if this guard has already completed.
    pub fn done_with(&mut self, detail: impl Display) {
        if let Some(start) = self.start.take() {
            self.recorder.record(&self.name, start.elapsed());
            self.phase.done_with(detail);
        }
    }
}

impl Drop for SubstepGuard {
    fn drop(&mut self) {
        if let Some(start) = self.start.take() {
            self.recorder.record(&self.name, start.elapsed());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substep_recorder_records_guard_drop() {
        let recorder = SubstepRecorder::default();
        {
            let _substep = recorder.substep("prune");
        }

        let timings = recorder.snapshot();
        let timing = timings.get("prune").expect("prune timing");
        assert_eq!(timing.invocations, 1);
    }

    #[test]
    fn substep_recorder_aggregates_repeated_names() {
        let recorder = SubstepRecorder::default();
        recorder.record("worker", Duration::from_millis(12));
        recorder.record("worker", Duration::from_millis(30));

        let snapshot = recorder.snapshot();
        assert_eq!(
            snapshot.get("worker"),
            Some(&SubstepTiming {
                duration_ms: 42,
                invocations: 2,
            }),
        );
        assert_eq!(
            serde_json::to_value(snapshot).unwrap(),
            serde_json::json!({
                "worker": {
                    "duration_ms": 42,
                    "invocations": 2,
                },
            }),
        );
    }

    #[test]
    fn substep_done_with_records_exactly_once() {
        let recorder = SubstepRecorder::default();
        let mut substep = recorder.substep("rank");
        substep.done_with("10 outputs");
        drop(substep);

        assert_eq!(recorder.snapshot().get("rank").unwrap().invocations, 1);
    }

    #[test]
    fn substep_recorder_aggregates_cloned_worker_recorders() {
        let recorder = SubstepRecorder::default();
        let workers = (0..4)
            .map(|_| {
                let recorder = recorder.clone();
                std::thread::spawn(move || {
                    let _substep = recorder.worker_substep("query");
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            worker.join().unwrap();
        }

        assert_eq!(recorder.snapshot().get("query").unwrap().invocations, 4);
    }
}
