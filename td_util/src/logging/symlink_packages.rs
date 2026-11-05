/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::Mutex;
use std::sync::OnceLock;

use tracing::info;

const ALIAS_LOG_LIMIT: usize = 200;

static ALIAS_LOGS: OnceLock<Mutex<AliasLogs>> = OnceLock::new();

#[derive(Default)]
struct AliasLogs {
    pairs: BTreeSet<(String, String)>,
    summary_logged: bool,
}

// A subscriber panic must not block package resolution.
impl AliasLogs {
    fn record(&mut self, alias: &str, canonical: &str) {
        if self.pairs.insert((alias.to_owned(), canonical.to_owned()))
            && self.pairs.len() <= ALIAS_LOG_LIMIT
        {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                info!("canonicalized symlink package {alias} -> {canonical}");
            }));
        }
    }

    fn finish(&mut self) {
        let beyond_cap = self.pairs.len().saturating_sub(ALIAS_LOG_LIMIT);
        if beyond_cap > 0 && !self.summary_logged {
            self.summary_logged = true;
            let _ = catch_unwind(AssertUnwindSafe(|| {
                info!("canonicalized symlink packages beyond log cap: {beyond_cap}");
            }));
        }
    }
}

pub fn record(alias: &str, canonical: &str) {
    ALIAS_LOGS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .record(alias, canonical);
}

pub(crate) fn finish() {
    if let Some(logs) = ALIAS_LOGS.get() {
        logs.lock()
            .unwrap_or_else(|error| error.into_inner())
            .finish();
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::io::Write;
    use std::sync::Arc;

    use super::*;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn caps_distinct_pairs_and_reports_the_remainder_once() {
        // With one live dispatcher, tracing-core caches a new callsite's interest from the
        // registering thread's default, so a parallel test could disable this callsite.
        let _second_dispatcher =
            tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_writer({
                let buffer = buffer.clone();
                move || buffer.clone()
            })
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let mut logs = AliasLogs::default();
            for index in 0..ALIAS_LOG_LIMIT + 3 {
                for _ in 0..2 {
                    logs.record(&format!("code//alias{index}"), "code//real");
                }
            }
            logs.finish();
            logs.finish();
        });
        let output = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        let expected = (0..ALIAS_LOG_LIMIT)
            .map(|index| {
                format!(" INFO canonicalized symlink package code//alias{index} -> code//real\n")
            })
            .chain(std::iter::once(
                " INFO canonicalized symlink packages beyond log cap: 3\n".to_owned(),
            ))
            .collect::<String>();
        assert_eq!(output, expected);
    }
}
