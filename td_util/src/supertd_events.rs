/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Simple interface for logging to the `supertd_events` dataset.

use std::sync::OnceLock;

use scuba::ScubaSampleBuilder;
pub use serde_json;
pub use tracing;

const SCUBA_DATASET: &str = "supertd_events";

static BUILDER: OnceLock<ScubaSampleBuilder> = OnceLock::new();

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
    TARGETS_SUCCESS,
    VERIFIABLE_MATCHER_SUCCESS,
    VERSE_SUCCESS,
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
    let mut builder = match std::env::var_os("SUPERTD_SCUBA_LOGFILE") {
        None => ScubaSampleBuilder::new(fb, SCUBA_DATASET),
        Some(path) => ScubaSampleBuilder::with_discard()
            .with_log_file(path)
            .unwrap(),
    };
    builder.add_common_server_data();
    add_sandcastle_columns(&mut builder);
    if BUILDER.set(builder).is_err() {
        tracing::error!("supertd_events Scuba client initialized twice");
    }
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
    ( event: $event:ident $(, $key:ident : $value:expr)* $(,)? ) => {
        let mut builder = $crate::supertd_events::sample_builder();
        builder.add("event", format!("{:?}", &$crate::supertd_events::Event::$event));
        $($crate::scuba! { @SET_FIELD(builder, $key, $value) })*
        if let Err(e) = builder.try_log() {
            $crate::supertd_events::tracing::error!(
                "Failed to log to supertd_events Scuba: {:?}", e);
        }
    };
    ( $($key:ident : $value:expr),* $(,)? ) => {
        compile_error!("`event` must be the first field in the `scuba!` macro");
    };
    ( @SET_FIELD ( $builder:ident, event, $value:expr ) ) => {
        compile_error!("duplicate `event` field in `scuba!` macro");
    };
    ( @SET_FIELD ( $builder:ident, data, $value:expr ) ) => {{
        use $crate::supertd_events::serde_json::json;
        match $crate::supertd_events::serde_json::to_string(&$value) {
            Ok(json) => {
                $builder.add("data", json);
            }
            Err(e) => {
                $crate::supertd_events::tracing::error!(
                    "Failed to serialize `data` column in `scuba!` macro: {:?}", e);
            }
        }
    }};
    ( @SET_FIELD ( $builder:ident, duration, $value:expr ) ) => {
        $builder.add("duration_ms", ::std::time::Duration::as_millis(&$value));
    };
    ( @SET_FIELD ( $builder:ident, sample_rate, $value:expr ) ) => {
        if let Some(sample_rate) = ::std::num::NonZeroU64::new($value) {
            $builder.sampled(sample_rate);
        } else {
            $crate::supertd_events::tracing::error!(
                "`sample_rate` must be nonzero in `scuba!` macro. This sample will always be logged.");
        }
    };
    ( @SET_FIELD ( $builder:ident, duration_ms, $value:expr ) ) => {
        compile_error!("unrecognized column name in `scuba!` macro: duration_ms (use `duration` instead)");
    };
    ( @SET_FIELD ( $builder:ident, $key:ident, $value:expr ) ) => {
        compile_error!(concat!("unrecognized column name in `scuba!` macro: ", stringify!($key)));
    };
}

/// Get the sample builder for the `supertd_events` dataset.
///
/// Please use the [`scuba!`] macro instead of this function, since it provides
/// additional type safety (e.g., prevents typos in column names). This function
/// is exposed only for internal use by the macro.
#[doc(hidden)]
pub fn sample_builder() -> ScubaSampleBuilder {
    BUILDER
        .get()
        .cloned()
        .unwrap_or_else(ScubaSampleBuilder::with_discard)
}

fn add_sandcastle_columns(sample: &mut ScubaSampleBuilder) {
    let Some(nexus_path) = std::env::var_os("SANDCASTLE_NEXUS") else {
        return;
    };
    let nexus_path = std::path::Path::new(&nexus_path);
    if !nexus_path.exists() {
        return;
    }
    let variables_path = nexus_path.join("variables");
    let variables = [
        "SANDCASTLE_ALIAS_NAME",
        "SANDCASTLE_ALIAS",
        "SANDCASTLE_COMMAND_NAME",
        "SANDCASTLE_INSTANCE_ID",
        "SANDCASTLE_IS_DRY_RUN",
        "SANDCASTLE_JOB_OWNER",
        "SANDCASTLE_NONCE",
        "SANDCASTLE_PHABRICATOR_DIFF_ID",
        "SANDCASTLE_SCHEDULE_TYPE",
        "SANDCASTLE_TYPE",
        "SANDCASTLE_URL",
        "SKYCASTLE_ACTION_ID",
        "SKYCASTLE_JOB_ID",
        "SKYCASTLE_WORKFLOW_RUN_ID",
        "STEP_IDX",
    ];
    for var in variables {
        let var_lowercase = var.to_ascii_lowercase();
        if let Ok(value) = std::fs::read_to_string(variables_path.join(var)) {
            sample.add(var_lowercase, value);
        } else if let Ok(value) = std::env::var(var) {
            sample.add(var_lowercase, value);
        }
    }
}

/// Flushes the `supertd_events` Scuba client when dropped.
///
/// Make sure this value is in scope for the duration of the program so that we
/// flush the client upon program exit.
#[must_use]
pub struct ScubaClientGuard(());

impl Drop for ScubaClientGuard {
    fn drop(&mut self) {
        if let Some(builder) = BUILDER.get() {
            if let Err(e) = builder.try_flush(std::time::Duration::from_secs(5)) {
                tracing::error!("Failed to flush supertd_events Scuba: {:?}", e);
            }
        }
    }
}
