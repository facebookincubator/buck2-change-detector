/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

#![forbid(unsafe_code)]
pub mod btd_graph_cache;
pub mod cli;
pub mod command;
pub mod executor;
pub mod file_io;
pub mod json;
pub mod knobs;
pub mod logging;
pub mod no_hash;
pub mod prelude;
pub mod project;
pub mod string;
pub mod supertd_events;
// @oss-disable: pub mod supertd_events_logger;
pub mod tracing;
pub mod workflow_error;
pub mod zstd;

/// Initialize `tracing` and `supertd_events` Scuba client.
///
/// Returns a guard that flushes the Scuba client when dropped.
///
/// # Panics
///
/// Panics if environment variable `SUPERTD_SCUBA_LOGFILE` is set and the log
/// file cannot be opened for writing.
pub fn init(fb: fbinit::FacebookInit) -> supertd_events::ScubaClientGuard {
    tracing::init_tracing();
    supertd_events::init(fb)
}

/// Initialize `tracing` with a specific log level and `supertd_events` Scuba client.
///
/// Returns a guard that flushes the Scuba client when dropped.
///
/// # Panics
///
/// Panics if environment variable `SUPERTD_SCUBA_LOGFILE` is set and the log
/// file cannot be opened for writing.
pub fn init_with_level(
    fb: fbinit::FacebookInit,
    level: tracing_subscriber::filter::LevelFilter,
) -> supertd_events::ScubaClientGuard {
    tracing::init_tracing_with_level(level);
    supertd_events::init(fb)
}
