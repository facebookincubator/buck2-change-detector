/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt::Display;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

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
