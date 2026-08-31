/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Record and replay external calls for deterministic offline TD runs.
//!
//! Replay never falls back to live calls: a missing key indicates divergence.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

use anyhow::anyhow;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tracing::error;
use tracing::info;

use crate::json::BUFFER_SIZE;

static CACHE: LazyLock<ExternalCallsCache> = LazyLock::new(ExternalCallsCache::new);

/// External-call namespace; identical keys in different variants do not collide.
#[derive(
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord
)]
pub enum CacheType {
    Gk,
    Interngraph,
    Configerator,
    DrValue,
    DrMinValue,
    JustKnobs,
}

/// Load a recorded cache and switch to replay mode: no external calls are made
/// from this point, and a call with no recorded result panics.
pub fn replay_external_calls(path: &Path) -> anyhow::Result<()> {
    CACHE.load_cache_for_replay(path)
}

/// Enables result retention for subsequent `save_cache` calls.
///
/// Must be called before the first wrapped call that should be recorded.
pub fn start_recording() {
    CACHE.start_recording();
}

/// Write everything recorded so far, for replaying a later run.
pub fn save_cache(path: &Path) -> anyhow::Result<()> {
    CACHE.save_cache(path)
}

/// Record or replay a fallible call. Failures are recorded too — see
/// [`Recorded`].
pub async fn try_call_cached<F, Fut, T>(
    cache_type: CacheType,
    key: &str,
    call: F,
) -> anyhow::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
    T: Serialize + for<'de> Deserialize<'de>,
{
    CACHE.try_call_cached(cache_type, key, call).await
}

/// The recorded outcome of a fallible call.
///
/// A failure is recorded rather than skipped. Callers of the fallible API fall
/// back on error and go on to finish successfully, so the recorded run's
/// behaviour includes the failure; dropping it would make the replay panic on a
/// missing key instead of reproducing the fallback. Only the message survives —
/// a replayed error is a fresh `anyhow::Error`, not the original type.
#[derive(Serialize, Deserialize)]
#[serde(tag = "outcome")]
enum Recorded<T> {
    Ok { value: T },
    Failed { error: String },
}

/// Record or replay an infallible async call.
pub async fn call_cached<F, Fut, T>(cache_type: CacheType, key: &str, call: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
    T: Serialize + for<'de> Deserialize<'de>,
{
    CACHE.call_cached(cache_type, key, call).await
}

/// Record or replay a synchronous call.
///
/// `key` is a closure because an inert run discards it: some callers read a knob
/// once per verifiable, and serializing a key only to drop it would be pure
/// overhead on every production run.
pub fn call_cached_sync<F, T, K>(cache_type: CacheType, key: K, call: F) -> T
where
    F: FnOnce() -> T,
    K: FnOnce() -> String,
    T: Serialize + for<'de> Deserialize<'de>,
{
    CACHE.call_cached_sync(cache_type, key, call)
}

/// `Off` is the normal production path: every call reaches its real
/// implementation. `Record` and `Replay` both memoize, so a value read twice in
/// one run is frozen at its first observation — that is what makes the
/// recording replayable, but it means a recorded run and an ordinary run can
/// disagree when the underlying value changes mid-run.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Mode {
    Off = 0,
    Record = 1,
    Replay = 2,
}

impl Mode {
    fn from_repr(value: u8) -> Self {
        match value {
            0 => Mode::Off,
            1 => Mode::Record,
            2 => Mode::Replay,
            other => unreachable!("mode is only ever stored from a Mode discriminant, got {other}"),
        }
    }
}

struct ExternalCallsCache {
    mode: AtomicU8,
    cache: Mutex<BTreeMap<CacheType, BTreeMap<String, Value>>>,
}

impl ExternalCallsCache {
    fn new() -> Self {
        Self {
            mode: AtomicU8::new(Mode::Off as u8),
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    fn mode(&self) -> Mode {
        Mode::from_repr(self.mode.load(Ordering::SeqCst))
    }

    /// Returns a cached value, panicking on a replay miss.
    ///
    /// An entry that is present but does not deserialize is corruption, not a
    /// miss: the recorded run wrote it from the same type this one is asking
    /// for, so a mismatch means the cache does not belong to this binary.
    /// Treating it as a miss would silently make a live call under replay.
    ///
    /// The cache lock is released before callers await a live request.
    fn lookup<T>(&self, cache_type: CacheType, key: &str) -> Option<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mode = self.mode();
        if mode == Mode::Off {
            return None;
        }

        let cached = self
            .cache
            .lock()
            .unwrap()
            .get(&cache_type)
            .and_then(|type_cache| type_cache.get(key).cloned());

        if let Some(value) = cached {
            return Some(serde_json::from_value::<T>(value).unwrap_or_else(|e| {
                panic!(
                    "corrupt external-calls cache: the entry for key {key} in \
                     {cache_type:?} does not deserialize into {}: {e}",
                    std::any::type_name::<T>()
                )
            }));
        }

        assert!(
            mode != Mode::Replay,
            "replay cache miss: no recorded value for key {key} in {cache_type:?}. \
             The replayed run reached a call the recorded run never made, so it has \
             already diverged."
        );

        None
    }

    /// A value that will not serialize is reported and skipped rather than
    /// panicking: recording runs in production, and a replay of this run fails
    /// closed on the missing key anyway.
    fn record<T: Serialize>(&self, cache_type: CacheType, key: &str, result: &T) {
        if self.mode() == Mode::Off {
            return;
        }
        match serde_json::to_value(result) {
            Ok(value) => {
                self.cache
                    .lock()
                    .unwrap()
                    .entry(cache_type)
                    .or_default()
                    .insert(key.to_owned(), value);
            }
            Err(e) => error!(
                "not recording {cache_type:?} key {key}: {} does not serialize: {e}. \
                 A replay of this run will fail closed when it reaches that call.",
                std::any::type_name::<T>()
            ),
        }
    }

    async fn try_call_cached<F, Fut, T>(
        &self,
        cache_type: CacheType,
        key: &str,
        call: F,
    ) -> anyhow::Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<T>>,
        T: Serialize + for<'de> Deserialize<'de>,
    {
        if let Some(recorded) = self.lookup::<Recorded<T>>(cache_type, key) {
            return match recorded {
                Recorded::Ok { value } => Ok(value),
                Recorded::Failed { error } => Err(anyhow!(error)),
            };
        }

        let result = call().await;
        // Borrowed, so recording a value does not require `T: Clone`.
        let recorded = match &result {
            Ok(value) => Recorded::Ok { value },
            Err(e) => Recorded::Failed {
                error: format!("{e:#}"),
            },
        };
        self.record(cache_type, key, &recorded);
        result
    }

    /// Infallible calls are recorded bare, without the [`Recorded`] envelope —
    /// they have no failure to distinguish.
    async fn call_cached<F, Fut, T>(&self, cache_type: CacheType, key: &str, call: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
        T: Serialize + for<'de> Deserialize<'de>,
    {
        if let Some(cached) = self.lookup::<T>(cache_type, key) {
            return cached;
        }

        let result = call().await;
        self.record(cache_type, key, &result);
        result
    }

    fn call_cached_sync<F, T, K>(&self, cache_type: CacheType, key: K, call: F) -> T
    where
        F: FnOnce() -> T,
        K: FnOnce() -> String,
        T: Serialize + for<'de> Deserialize<'de>,
    {
        if self.mode() == Mode::Off {
            return call();
        }

        let key = key();
        if let Some(cached) = self.lookup::<T>(cache_type, &key) {
            return cached;
        }

        let result = call();
        self.record(cache_type, &key, &result);
        result
    }

    /// Replay wins when it starts first: both cache files may be supplied at once
    /// (the backtest rerun script does), and `start_recording` then stays a no-op.
    /// Starting replay after recording began is rejected instead of discarding
    /// the entries already recorded.
    fn start_recording(&self) {
        let _ = self.mode.compare_exchange(
            Mode::Off as u8,
            Mode::Record as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    fn load_cache_for_replay(&self, path: &Path) -> anyhow::Result<()> {
        match self.mode() {
            Mode::Off => {}
            Mode::Record => bail!("cannot enter replay mode after recording has started"),
            Mode::Replay => bail!("already in replay mode"),
        }

        let reader = std::io::BufReader::new(File::open(path)?);
        let loaded: BTreeMap<CacheType, BTreeMap<String, Value>> = serde_json::from_reader(reader)?;
        *self.cache.lock().unwrap() = loaded;
        self.mode.store(Mode::Replay as u8, Ordering::SeqCst);

        info!("Loaded external calls cache from {:?}", path);
        Ok(())
    }

    fn save_cache(&self, path: &Path) -> anyhow::Result<()> {
        let snapshot = self.cache.lock().unwrap().clone();
        let file = File::create(path)?;
        let mut writer = BufWriter::with_capacity(BUFFER_SIZE, file);
        serde_json::to_writer(&mut writer, &snapshot)?;
        writer.flush()?;

        info!("Saved external calls cache to {:?}", path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use rstest::rstest;
    use tempfile::NamedTempFile;

    use super::*;

    fn local() -> ExternalCallsCache {
        ExternalCallsCache::new()
    }

    #[test]
    fn test_inert_by_default_does_not_memoise() {
        let cache = local();
        let calls = AtomicU32::new(0);

        for _ in 0..3 {
            cache.call_cached_sync(
                CacheType::JustKnobs,
                || "some/knob:on".to_owned(),
                || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    true
                },
            );
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "every call must reach the real implementation when no cache was requested"
        );
    }

    /// Callers build a key per read, so an inert run must not pay for one.
    #[test]
    fn test_an_inert_read_does_not_build_a_cache_key() {
        let cache = local();

        let value = cache.call_cached_sync(
            CacheType::JustKnobs,
            || panic!("the cache key must not be built while the recorder is inert"),
            || true,
        );

        assert!(value, "the real implementation still supplies the value");
    }

    #[test]
    fn test_recording_must_be_started_to_memoise() {
        let cache = local();
        let calls = AtomicU32::new(0);
        let call = || {
            calls.fetch_add(1, Ordering::SeqCst);
            true
        };
        let key = || "k".to_owned();

        cache.call_cached_sync(CacheType::JustKnobs, key, call);
        cache.start_recording();
        cache.call_cached_sync(CacheType::JustKnobs, key, call);
        cache.call_cached_sync(CacheType::JustKnobs, key, call);

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one call before recording started, one after; the third is served from the cache"
        );
    }

    #[test]
    fn test_replay_after_recording_is_rejected_without_discarding_entries() {
        let cache = local();
        cache.start_recording();
        let calls = AtomicU32::new(0);
        let key = || "k".to_owned();
        let call = || {
            calls.fetch_add(1, Ordering::SeqCst);
            "recorded".to_owned()
        };
        cache.call_cached_sync(CacheType::Configerator, key, call);

        let file = NamedTempFile::new().unwrap();
        serde_json::to_writer(
            File::create(file.path()).unwrap(),
            &BTreeMap::<CacheType, BTreeMap<String, Value>>::new(),
        )
        .unwrap();
        let error = cache.load_cache_for_replay(file.path()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "cannot enter replay mode after recording has started"
        );
        assert_eq!(
            cache.call_cached_sync(CacheType::Configerator, key, call),
            "recorded"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_records_once_then_serves_from_cache() {
        let cache = local();
        cache.start_recording();
        let calls = AtomicU32::new(0);
        let call = || {
            calls.fetch_add(1, Ordering::SeqCst);
            "value".to_owned()
        };

        let key = || "k".to_owned();
        let first = cache.call_cached_sync(CacheType::Configerator, key, call);
        let second = cache.call_cached_sync(CacheType::Configerator, key, call);

        assert_eq!((first, second), ("value".to_owned(), "value".to_owned()));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second lookup must be served from the cache, not re-called"
        );
    }

    #[test]
    fn test_same_key_in_different_types_does_not_collide() {
        let cache = local();
        cache.start_recording();
        cache.call_cached_sync(
            CacheType::Configerator,
            || "shared".to_owned(),
            || "config".to_owned(),
        );
        let other =
            cache.call_cached_sync(CacheType::Gk, || "shared".to_owned(), || "gk".to_owned());

        assert_eq!(other, "gk", "each cache type is its own key namespace");
    }

    #[test]
    #[should_panic(expected = "replay cache miss")]
    fn test_replay_miss_is_fatal() {
        let cache = local();
        let file = NamedTempFile::new().unwrap();
        serde_json::to_writer(
            File::create(file.path()).unwrap(),
            &BTreeMap::from([(CacheType::Gk, BTreeMap::from([("known", "yes")]))]),
        )
        .unwrap();
        cache.load_cache_for_replay(file.path()).unwrap();

        assert_eq!(
            cache.call_cached_sync(CacheType::Gk, || "known".to_owned(), || "unused".to_owned()),
            "yes"
        );
        cache.call_cached_sync(CacheType::Gk, || "unknown".to_owned(), || "live".to_owned());
    }

    #[test]
    fn test_round_trip_through_a_file() {
        let recorder = local();
        recorder.start_recording();
        recorder.call_cached_sync(CacheType::DrValue, || "controller".to_owned(), || 1.25_f64);
        recorder.call_cached_sync(
            CacheType::JustKnobs,
            || "some/knob:on".to_owned(),
            || Some(true),
        );
        recorder.call_cached_sync(
            CacheType::JustKnobs,
            || "some/knob:absent".to_owned(),
            || Option::<bool>::None,
        );

        let file = NamedTempFile::new().unwrap();
        recorder.save_cache(file.path()).unwrap();

        let replayer = local();
        replayer.load_cache_for_replay(file.path()).unwrap();

        let dr: f64 = replayer.call_cached_sync(
            CacheType::DrValue,
            || "controller".to_owned(),
            || panic!("must come from the cache"),
        );
        let knob: Option<bool> = replayer.call_cached_sync(
            CacheType::JustKnobs,
            || "some/knob:on".to_owned(),
            || panic!("must come from the cache"),
        );
        let absent: Option<bool> = replayer.call_cached_sync(
            CacheType::JustKnobs,
            || "some/knob:absent".to_owned(),
            || panic!("must come from the cache"),
        );

        assert_eq!(dr, 1.25);
        assert_eq!(knob, Some(true));
        assert_eq!(
            absent, None,
            "an absent knob must replay as absent, not as some caller's default"
        );
    }

    #[rstest]
    #[case::success(Ok("available"))]
    #[case::failure(Err("cruise control unreachable"))]
    fn test_fallible_call_replays_the_recorded_outcome(
        #[case] outcome: Result<&'static str, &'static str>,
    ) {
        fn normalize(result: anyhow::Result<String>) -> Result<String, String> {
            result.map_err(|error| error.to_string())
        }

        let expected = outcome.map(str::to_owned).map_err(str::to_owned);
        let recorder = local();
        recorder.start_recording();
        let recorded = futures::executor::block_on(recorder.try_call_cached(
            CacheType::DrValue,
            "controller",
            || async move { outcome.map(str::to_owned).map_err(|error| anyhow!(error)) },
        ));
        assert_eq!(normalize(recorded), expected);

        let file = NamedTempFile::new().unwrap();
        recorder.save_cache(file.path()).unwrap();

        let replayer = local();
        replayer.load_cache_for_replay(file.path()).unwrap();
        let replayed = futures::executor::block_on(replayer.try_call_cached(
            CacheType::DrValue,
            "controller",
            || async { panic!("replay must not make a live call") },
        ));

        assert_eq!(normalize(replayed), expected);
    }

    #[test]
    #[should_panic(expected = "corrupt external-calls cache")]
    fn test_an_entry_of_the_wrong_type_is_corruption_not_a_miss() {
        let cache = local();
        let file = NamedTempFile::new().unwrap();
        serde_json::to_writer(
            File::create(file.path()).unwrap(),
            &BTreeMap::from([(CacheType::Gk, BTreeMap::from([("k", "not a number")]))]),
        )
        .unwrap();
        cache.load_cache_for_replay(file.path()).unwrap();

        // Treating this as a miss would make a live call under replay.
        cache.call_cached_sync(CacheType::Gk, || "k".to_owned(), || 1.0_f64);
    }
}
