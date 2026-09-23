/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Bounded errors for terminal operation events.

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct OperationError {
    pub phase: Option<&'static str>,
    pub code: &'static str,
    pub message: String,
}

impl OperationError {
    pub fn new(phase: Option<&'static str>, code: &'static str, error: &anyhow::Error) -> Self {
        const LIMIT: usize = 1024;
        const PREFIX: &str = "... [truncated]\n";
        let mut message = format!("{error:#}");
        if message.len() > LIMIT {
            let mut start = message.len() - (LIMIT - PREFIX.len());
            while !message.is_char_boundary(start) {
                start += 1;
            }
            message = format!("{PREFIX}{}", &message[start..]);
        }
        Self {
            phase,
            code,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_preserves_the_cause_with_a_utf8_byte_bound() {
        let error = anyhow::anyhow!("{}root cause", "🦀".repeat(1024));
        let event = OperationError::new(Some("write"), "write_failed", &error);
        assert!(event.message.len() <= 1024);
        assert!(event.message.ends_with("root cause"));
        assert!(event.message.starts_with("... [truncated]\n"));
    }
}
