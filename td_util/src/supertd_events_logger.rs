/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Simple interface for logging to the `supertd_events` dataset.

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(not(target_os = "linux"))]
pub use non_linux::*;

#[cfg(not(target_os = "linux"))]
mod non_linux {
    use std::env;
    use std::fs::OpenOptions;
    use std::io::BufWriter as StdBufWriter;
    use std::io::Write;
    use std::process::Child;
    use std::process::ChildStdin;
    use std::process::Command;
    use std::process::Stdio;
    use std::sync::LazyLock;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use build_info::BuildInfo;
    use indexmap::IndexMap;
    use serde::Serialize;
    use serde_json::Number as JsonNumber;
    use serde_json::Value as JsonValue;

    /// Scribe category that routes to the `supertd_events` Scuba dataset.
    /// Mirrors linttool's convention (`perfpipe_linttool_events` →
    /// `linttool_events` Scuba). Verified end-to-end via a hand-crafted
    /// sample on 2026-05-14 (round-tripped to Scuba in <60s).
    const SCUBA_CATEGORY: &str = "perfpipe_supertd_events";

    /// Logger sink: either a `scribe_cat` subprocess, or a file when
    /// `SUPERTD_SCUBA_LOGFILE` is set, or `None` if logging is disabled
    /// (binary not found on PATH, or `NOSUPERTDLOG=1`).
    enum Sink {
        ScribeCat {
            // Held to keep the child alive for the lifetime of the static
            // LOGGER. The child is reaped via stdin-EOF when the parent
            // process exits and the OS closes our end of the pipe; we do
            // not (and cannot — statics never run Drop) call kill_on_drop.
            _child: Child,
            stdin: Mutex<StdBufWriter<ChildStdin>>,
        },
        File {
            writer: Mutex<StdBufWriter<std::fs::File>>,
        },
    }

    static LOGGER: OnceLock<Sink> = OnceLock::new();

    /// Initialize the `supertd_events` Scuba client for non-Linux platforms.
    ///
    /// On Mac (and other non-Linux), there is no thrift-codegen Scuba logger
    /// available, so we mirror the linttool pattern (see
    /// `fbcode/linttool/linttool/src/facebook/logger.rs`): spawn `scribe_cat
    /// supertd_events` as a subprocess and pipe JSON-serialized samples to its
    /// stdin. `scribe_cat` is shipped to Mac laptops at Meta.
    ///
    /// Honors:
    /// - `SUPERTD_SCUBA_LOGFILE`: write JSONL to that file instead (test escape hatch).
    /// - `NOSUPERTDLOG=1`: do not initialize anything (mirrors `NOLINTTOOLLOG`).
    pub fn init(_fb: fbinit::FacebookInit) {
        if env::var_os("NOSUPERTDLOG").is_some_and(|v| v == "1") {
            return;
        }

        let sink = if let Some(path) = env::var_os("SUPERTD_SCUBA_LOGFILE") {
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => Some(Sink::File {
                    writer: Mutex::new(StdBufWriter::new(f)),
                }),
                Err(e) => {
                    tracing::warn!(
                        path = ?path,
                        error = ?e,
                        "supertd_events: failed to open SUPERTD_SCUBA_LOGFILE"
                    );
                    None
                }
            }
        } else {
            spawn_scribe_cat()
        };

        if let Some(sink) = sink {
            if LOGGER.set(sink).is_err() {
                tracing::warn!("supertd_events: client initialized twice; ignoring second init");
            }
        }
    }

    fn spawn_scribe_cat() -> Option<Sink> {
        let prog = which::which("scribe_cat").unwrap_or_else(|_| "scribe_cat".into());
        let mut command = Command::new(&prog);
        command
            .arg(SCUBA_CATEGORY)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    program = ?prog,
                    error = ?e,
                    "supertd_events: failed to spawn scribe_cat; events will be dropped"
                );
                return None;
            }
        };

        let stdin = child.stdin.take()?;
        Some(Sink::ScribeCat {
            _child: child,
            stdin: Mutex::new(StdBufWriter::new(stdin)),
        })
    }

    /// A Scuba sample in the JSON wire format that `scribe_cat` accepts. Pure
    /// data — no I/O, fully cross-platform. Layout mirrors
    /// `fbcode/linttool/linttool/src/facebook/scuba.rs`.
    #[derive(Clone, Debug, Serialize)]
    pub struct ScubaSample {
        int: IndexMap<String, JsonValue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        normal: Option<IndexMap<String, JsonValue>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        normvector: Option<IndexMap<String, JsonValue>>,
    }

    impl ScubaSample {
        fn new() -> Self {
            let seconds_since_epoch = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut sample = ScubaSample {
                int: IndexMap::new(),
                normal: None,
                normvector: None,
            };
            sample.int.insert(
                "time".to_owned(),
                JsonValue::Number(JsonNumber::from(seconds_since_epoch)),
            );
            sample
        }

        /// Set an integer column.
        pub fn set_int(&mut self, key: &str, value: i64) {
            self.int
                .insert(key.to_owned(), JsonValue::Number(JsonNumber::from(value)));
        }

        /// Set a normal (string) column.
        pub fn set_normal(&mut self, key: &str, value: String) {
            self.normal
                .get_or_insert_with(IndexMap::new)
                .insert(key.to_owned(), JsonValue::String(value));
        }
    }

    /// Build a fresh sample, prepopulated with platform/host context.
    #[doc(hidden)]
    pub fn log_entry() -> ScubaSample {
        let mut sample = ScubaSample::new();
        if let Some(host) = HOSTNAME.as_deref() {
            sample.set_normal("server_hostname", host.to_owned());
        }
        if let Some(user) = USERNAME.as_deref() {
            sample.set_normal("user", user.to_owned());
        }
        sample.set_normal("operating_system", std::env::consts::OS.to_owned());
        sample.set_normal("build_revision", BuildInfo::get_revision().to_owned());
        sample.set_normal("build_rule", BuildInfo::get_rule().to_owned());
        sample
    }

    /// Write a sample to the underlying sink. Errors are logged via `tracing`
    /// and otherwise swallowed (telemetry is fire-and-forget).
    #[doc(hidden)]
    pub fn log(sample: &ScubaSample) {
        let Some(sink) = LOGGER.get() else {
            return;
        };
        let json = match serde_json::to_string(sample) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = ?e, "supertd_events: failed to serialize sample");
                return;
            }
        };
        let result = match sink {
            Sink::ScribeCat { stdin, .. } => write_line(stdin, &json),
            Sink::File { writer } => write_line(writer, &json),
        };
        if let Err(e) = result {
            tracing::warn!(error = ?e, "supertd_events: failed to write sample");
        }
    }

    fn write_line<W: Write>(writer: &Mutex<StdBufWriter<W>>, json: &str) -> std::io::Result<()> {
        let mut w = writer
            .lock()
            .map_err(|_| std::io::Error::other("supertd_events writer mutex poisoned"))?;
        w.write_all(json.as_bytes())?;
        w.write_all(b"\n")?;
        w.flush()
    }

    static HOSTNAME: LazyLock<Option<String>> =
        LazyLock::new(|| hostname::get().ok().and_then(|h| h.into_string().ok()));

    static USERNAME: LazyLock<Option<String>> =
        LazyLock::new(|| env::var("USER").ok().or_else(|| env::var("LOGNAME").ok()));

    /// Mac-side `scuba_logger!` macro. Mirrors the Linux macro's API surface so
    /// `td_util::scuba!{ event: X, duration: t.elapsed(), data: json!({...}) }`
    /// compiles and behaves identically on both platforms.
    #[macro_export]
    macro_rules! scuba_logger {
        ( event: $event:ident $(, $key:ident : $value:expr)* $(,)? ) => {{
            let mut sample = $crate::supertd_events_logger::log_entry();
            sample.set_normal(
                "event",
                format!("{:?}", &$crate::supertd_events::Event::$event),
            );
            $($crate::scuba_logger! { @SET_FIELD(sample, $key, $value) })*
            $crate::supertd_events_logger::log(&sample);
        }};
        ( $($key:ident : $value:expr),* $(,)? ) => {
            compile_error!("`event` must be the first field in the `scuba!` macro");
        };
        ( @SET_FIELD ( $sample:ident, event, $value:expr ) ) => {
            compile_error!("duplicate `event` field in `scuba!` macro");
        };
        ( @SET_FIELD ( $sample:ident, data, $value:expr ) ) => {{
            #[allow(unused_imports)]
            use $crate::supertd_events::serde_json::json;
            match $crate::supertd_events::serde_json::to_string(&$value) {
                Ok(json) => $sample.set_normal("data", json),
                Err(e) => {
                    $crate::supertd_events::tracing::error!(
                        "Failed to serialize `data` column in `scuba!` macro: {:?}", e);
                }
            }
        }};
        ( @SET_FIELD ( $sample:ident, duration, $value:expr ) ) => {
            $sample.set_int(
                "duration_ms",
                ::std::time::Duration::as_millis(&$value) as i64,
            );
        };
        ( @SET_FIELD ( $sample:ident, duration_ms, $value:expr ) ) => {
            compile_error!("unrecognized column name in `scuba!` macro: duration_ms (use `duration` instead)");
        };
        ( @SET_FIELD ( $sample:ident, $key:ident, $value:expr ) ) => {
            compile_error!(concat!("unrecognized column name in `scuba!` macro: ", stringify!($key)));
        };
    }

    #[cfg(test)]
    mod tests {
        use std::io::BufRead;
        use std::io::BufReader;

        use tempfile::NamedTempFile;

        use super::*;

        fn fixed_sample() -> ScubaSample {
            let mut sample = ScubaSample {
                int: IndexMap::new(),
                normal: None,
                normvector: None,
            };
            sample.int.insert(
                "time".to_owned(),
                JsonValue::Number(JsonNumber::from(123u64)),
            );
            sample
        }

        #[test]
        fn test_sample_serialization_minimal() {
            let sample = fixed_sample();
            assert_eq!(
                serde_json::to_value(&sample).unwrap(),
                serde_json::json!({"int": {"time": 123}}),
            );
        }

        #[test]
        fn test_sample_serialization_mixed() {
            let mut sample = fixed_sample();
            sample.set_int("duration_ms", 42);
            sample.set_normal("event", "BTD_SUCCESS".to_owned());
            sample.set_normal("data", "{\"k\":1}".to_owned());

            assert_eq!(
                serde_json::to_value(&sample).unwrap(),
                serde_json::json!({
                    "int": {"time": 123, "duration_ms": 42},
                    "normal": {"event": "BTD_SUCCESS", "data": "{\"k\":1}"},
                }),
            );
        }

        /// End-to-end test of the file-sink path: write two samples, read the
        /// file back, assert each line round-trips through serde. Covers the
        /// `OpenOptions` + `BufWriter::flush` + JSONL framing plumbing.
        #[test]
        fn test_file_sink_round_trip() {
            let tmp = NamedTempFile::new().expect("tempfile");
            let writer = StdBufWriter::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(tmp.path())
                    .expect("open"),
            );
            let sink = Sink::File {
                writer: Mutex::new(writer),
            };

            let mut s1 = fixed_sample();
            s1.set_normal("event", "ONE".to_owned());
            let mut s2 = fixed_sample();
            s2.set_normal("event", "TWO".to_owned());

            for s in [&s1, &s2] {
                let json = serde_json::to_string(s).unwrap();
                let writer = match &sink {
                    Sink::File { writer } => writer,
                    _ => unreachable!(),
                };
                write_line(writer, &json).expect("write");
            }

            let lines: Vec<String> =
                BufReader::new(std::fs::File::open(tmp.path()).expect("reopen"))
                    .lines()
                    .collect::<Result<_, _>>()
                    .expect("read lines");

            assert_eq!(lines.len(), 2);
            let v1: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
            let v2: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
            assert_eq!(v1["normal"]["event"], "ONE");
            assert_eq!(v2["normal"]["event"], "TWO");
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::env::var;
    use std::path::Path;
    use std::sync::OnceLock;

    use build_info::BuildInfo;
    use supertd_events_rust_logger::SupertdEventsLogEntry;
    use supertd_events_rust_logger::SupertdEventsLogger;

    static LOG_ENTRY: OnceLock<SupertdEventsLogEntry> = OnceLock::new();
    static FB_INIT: OnceLock<fbinit::FacebookInit> = OnceLock::new();

    /// Initialize the Scuba client for the `supertd_events` dataset.
    ///
    /// Returns a guard that flushes the Scuba client when dropped.
    ///
    /// Expects `tracing` to be initialized.
    pub fn init(fb: fbinit::FacebookInit) {
        if FB_INIT.set(fb).is_err() {
            tracing::error!("supertd_events client initialized twice");
        }
        let mut log_entry = SupertdEventsLogEntry::default();
        add_common_server_data(&mut log_entry);
        add_sandcastle_columns(&mut log_entry);
        if LOG_ENTRY.set(log_entry).is_err() {
            tracing::error!("supertd_events Scuba client initialized twice");
        }
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
    macro_rules! scuba_logger {
    ( event: $event:ident $(, $key:ident : $value:expr)* $(,)? ) => {
        let mut builder = $crate::supertd_events_logger::log_entry();
        builder.set_event(format!("{:?}", &$crate::supertd_events::Event::$event));
        $($crate::scuba_logger! { @SET_FIELD(builder, $key, $value) })*
        $crate::supertd_events_logger::log(&builder);
    };
    ( $($key:ident : $value:expr),* $(,)? ) => {
        compile_error!("`event` must be the first field in the `scuba!` macro");
    };
    ( @SET_FIELD ( $builder:ident, event, $value:expr ) ) => {
        compile_error!("duplicate `event` field in `scuba!` macro");
    };
    ( @SET_FIELD ( $builder:ident, data, $value:expr ) ) => {{
        #[allow(unused_imports)]
        use $crate::supertd_events::serde_json::json;

        match $crate::supertd_events::serde_json::to_string(&$value) {
            Ok(json) => {
                $builder.set_data(json);
            }
            Err(e) => {
                $crate::supertd_events::tracing::error!(
                    "Failed to serialize `data` column in `scuba!` macro: {:?}", e);
            }
        }
    }};
    ( @SET_FIELD ( $builder:ident, duration, $value:expr ) ) => {
        $builder.set_duration_ms(::std::time::Duration::as_millis(&$value) as i64);
    };
    ( @SET_FIELD ( $builder:ident, duration_ms, $value:expr ) ) => {
        compile_error!("unrecognized column name in `scuba!` macro: duration_ms (use `duration` instead)");
    };
    ( @SET_FIELD ( $builder:ident, $key:ident, $value:expr ) ) => {
        compile_error!(concat!("unrecognized column name in `scuba!` macro: ", stringify!($key)));
    };
}

    /// Get the log_entry for the `supertd_events` dataset.
    ///
    /// Please use the [`scuba!`] macro instead of this function, since it provides
    /// additional type safety (e.g., prevents typos in column names). This function
    /// is exposed only for internal use by the macro.
    #[doc(hidden)]
    pub fn log_entry() -> SupertdEventsLogEntry {
        LOG_ENTRY.get().cloned().unwrap_or_default()
    }

    #[doc(hidden)]
    pub fn log(log_entry: &SupertdEventsLogEntry) {
        if let Some(&fb) = FB_INIT.get() {
            if let Err(e) = SupertdEventsLogger::from_entry(fb, log_entry).log() {
                tracing::error!("Failed to flush supertd_events Scuba: {:?}", e);
            }
        }
    }

    fn add_common_server_data(log_entry: &mut SupertdEventsLogEntry) {
        if let Ok(who) = fbwhoami::FbWhoAmI::get() {
            if let Some(hostname) = who.name.as_deref() {
                log_entry.set_server_hostname(hostname.to_owned());
            }
            if let Some(region) = who.region.as_deref() {
                log_entry.set_region(region.to_owned());
            }
            if let Some(dc) = who.datacenter.as_deref() {
                log_entry.set_datacenter(dc.to_owned());
            }
            if let Some(dc_prefix) = who.region_datacenter_prefix.as_deref() {
                log_entry.set_region_datacenter_prefix(dc_prefix.to_owned());
            }
        }

        if let Ok(smc_tier) = var("SMC_TIERS") {
            log_entry.set_server_tier(smc_tier);
        }

        if let Ok(tw_task_id) = var("TW_TASK_ID") {
            log_entry.set_tw_task_id(tw_task_id);
        }

        if let Ok(tw_canary_id) = var("TW_CANARY_ID") {
            log_entry.set_tw_canary_id(tw_canary_id);
        }

        if let (Ok(tw_cluster), Ok(tw_user), Ok(tw_name)) = (
            var("TW_JOB_CLUSTER"),
            var("TW_JOB_USER"),
            var("TW_JOB_NAME"),
        ) {
            log_entry.set_tw_handle(format!("{}/{}/{}", tw_cluster, tw_user, tw_name));
        };

        if let (Ok(tw_cluster), Ok(tw_user), Ok(tw_name), Ok(tw_task_id)) = (
            var("TW_JOB_CLUSTER"),
            var("TW_JOB_USER"),
            var("TW_JOB_NAME"),
            var("TW_TASK_ID"),
        ) {
            log_entry.set_tw_task_handle(format!(
                "{}/{}/{}/{}",
                tw_cluster, tw_user, tw_name, tw_task_id
            ));
        };

        #[cfg(target_os = "linux")]
        {
            log_entry.set_build_revision(BuildInfo::get_revision().to_owned());
            log_entry.set_build_rule(BuildInfo::get_rule().to_owned());
        }

        #[cfg(target_os = "linux")]
        log_entry.set_operating_system("linux".to_owned());

        #[cfg(target_os = "macos")]
        log_entry.set_operating_system("macos".to_owned());

        #[cfg(target_os = "windows")]
        log_entry.set_operating_system("windows".to_owned());
    }

    fn apply_verifiable(var: &str, variables_path: &Path, f: impl FnOnce(String)) {
        if let Ok(value) = std::fs::read_to_string(variables_path.join(var)) {
            f(value);
        } else if let Ok(value) = std::env::var(var) {
            f(value);
        }
    }

    fn add_sandcastle_columns(log_entry: &mut SupertdEventsLogEntry) {
        let Some(nexus_path) = std::env::var_os("SANDCASTLE_NEXUS") else {
            return;
        };
        let nexus_path = std::path::Path::new(&nexus_path);
        if !nexus_path.exists() {
            return;
        }
        let variables_path = nexus_path.join("variables");
        apply_verifiable("SANDCASTLE_ALIAS_NAME", &variables_path, |value| {
            log_entry.set_sandcastle_alias_name(value);
        });
        apply_verifiable("SANDCASTLE_ALIAS", &variables_path, |value| {
            log_entry.set_sandcastle_alias(value);
        });
        apply_verifiable("SANDCASTLE_COMMAND_NAME", &variables_path, |value| {
            log_entry.set_sandcastle_command_name(value);
        });
        apply_verifiable("SANDCASTLE_INSTANCE_ID", &variables_path, |value| {
            log_entry.set_sandcastle_instance_id(value);
        });
        apply_verifiable("SANDCASTLE_IS_DRY_RUN", &variables_path, |value| {
            log_entry.set_sandcastle_is_dry_run(value);
        });
        apply_verifiable("SANDCASTLE_JOB_OWNER", &variables_path, |value| {
            log_entry.set_sandcastle_job_owner(value);
        });
        apply_verifiable("SANDCASTLE_NONCE", &variables_path, |value| {
            log_entry.set_sandcastle_nonce(value);
        });
        apply_verifiable("SANDCASTLE_PHABRICATOR_DIFF_ID", &variables_path, |value| {
            log_entry.set_sandcastle_phabricator_diff_id(value);
        });
        apply_verifiable("SANDCASTLE_SCHEDULE_TYPE", &variables_path, |value| {
            log_entry.set_sandcastle_schedule_type(value);
        });
        apply_verifiable("SANDCASTLE_TYPE", &variables_path, |value| {
            log_entry.set_sandcastle_type(value);
        });
        apply_verifiable("SANDCASTLE_URL", &variables_path, |value| {
            log_entry.set_sandcastle_url(value);
        });
        apply_verifiable("SKYCASTLE_ACTION_ID", &variables_path, |value| {
            log_entry.set_skycastle_action_id(value);
        });
        apply_verifiable("SKYCASTLE_JOB_ID", &variables_path, |value| {
            log_entry.set_skycastle_job_id(value);
        });
        apply_verifiable("SKYCASTLE_WORKFLOW_RUN_ID", &variables_path, |value| {
            log_entry.set_skycastle_workflow_run_id(value);
        });
    }
}
