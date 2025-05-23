/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Simple interface for logging to the `supertd_events` dataset.

pub use serde_json;
pub use tracing;

/// All events logged to the `supertd_events` dataset.
///
/// Each event should generally be logged from a single source location.
#[derive(Debug)]
#[allow(non_camel_case_types)]
pub enum Event {
    BTD_SUCCESS,
    CITRACE_ARGS_PARSED,
    INVALID_TRIGGER,
    RANKER_SUCCESS,
    SCHEDULER_SUCCESS,
    SCHEDULER_FAILURE,
    TARGETS_SUCCESS,
    VERIFIABLE_MATCHER_SUCCESS,
    VERSE_SUCCESS,
    VERSE_PG_DEMAND_RESPONSE_VERIFIABLES_COUNT,
    VERSE_PG_GUARANTEED_VERIFIABLES_COUNT,
    VERSE_MULTISTAGE_SUCCESS,
    VERSE_MULTISTAGE_FAILURE,
    BUILD_DIRECTIVES_SPECIFIED,
    RE_METADATA_SUCCESS,
    GENERATED_TARGETS_COUNT,
    QE_CHECK,
    RUNWAY_RELATES_CALL_FAILURE,
    TARGETS_WITHOUT_BUDGET_ENTITY,
    SCUBA_TARGET_LOGGING_FAILURE,
}

#[derive(Debug)]
#[allow(non_camel_case_types)]
#[derive(serde::Serialize)]
pub enum Step {
    AUDIT,
    TARGETS,
    BTD,
    VERIFIABLE_MATCHER,
    RANKER,
    VERSE,
    SCHEDULER,
    RERUN,
}

/// Initialize the Scuba client for the `supertd_events` dataset.
///
/// Returns a guard that flushes the Scuba client when dropped.
///
/// Expects `tracing` to be initialized.
///
/// If the environment variable `SUPERTD_SCUBA_LOGFILE` is set, then log to that
/// filename instead of Scuba (useful for testing).
///
/// # Panics
///
/// Panics if `SUPERTD_SCUBA_LOGFILE` is set and the log file cannot be opened
/// for writing.
pub fn init(fb: fbinit::FacebookInit) -> ScubaClientGuard {
    // @oss-disable: crate::supertd_events_logger::init(fb);

    ScubaClientGuard(())
}

/// Log a sample to the `supertd_events` dataset.
///
/// The `event` column should be a distinct string for each source location
/// logging an event.
///
/// The `data` column contains JSON-encoded data specific to that event (so that
/// we do not inflate the number of columns in the Scuba table with properties
/// populated by only one event). Use this data in derived columns or queries
/// using `JSON_EXTRACT`.
///
/// If [`init`] has not been invoked, the sample will not be logged.
///
/// # Examples
///
/// ```
/// # let f = || (10, 2);
/// let t = std::time::Instant::now();
/// let (foos_run, bars_launched) = f();
/// td_util::scuba!(
///     event: BTD_SUCCESS,
///     duration: t.elapsed(),
///     data: json!({
///         "arbitrary": ["JSON", "object"],
///         "foos_run": foos_run,
///         "bars_launched": bars_launched,
///     })
/// );
/// ```
#[macro_export]
macro_rules! scuba {
    ( event: $event:ident $(, $key:ident : $value:expr_2021)* $(,)? ) => {
        // @oss-disable: $crate::scuba_logger! {event: $event $(, $key : $value)*};
    };
    ( $($key:ident : $value:expr_2021),* $(,)? ) => {
        compile_error!("`event` must be the first field in the `scuba!` macro");
    };
}

/// Flushes the `supertd_events` Scuba client when dropped.
///
/// Make sure this value is in scope for the duration of the program so that we
/// flush the client upon program exit.
#[must_use]
pub struct ScubaClientGuard(());
