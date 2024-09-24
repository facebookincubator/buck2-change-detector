/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

#![forbid(unsafe_code)]

pub mod cli;
pub mod command;
pub mod directives;
pub mod executor;
pub mod file_writer;
pub mod json;
pub mod knobs;
pub mod no_hash;
pub mod prelude;
pub mod project;
pub mod schedules;
pub mod string;
pub mod supertd_events;
pub mod tracing;
pub mod workflow_error;
pub mod xplat;

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
