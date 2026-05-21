/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Thin shell-out wrappers around the `sl` CLI for callers that need
//! read-only status / log queries without depending on a full Sapling
//! Rust client. Each function spawns `sl` via `std::process::Command`.

use std::process::Command;

/// Run `sl status --rev FROM --rev TO`, returning the raw stdout.
/// Errors if `sl` fails to spawn or exits non-zero.
pub fn sl_status(from_hash: &str, to_hash: &str) -> anyhow::Result<String> {
    let output = Command::new("sl")
        .args(["status", "--rev", from_hash, "--rev", to_hash])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "sl status --rev {from_hash} --rev {to_hash} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Returns the exclusive commit distance between `from_hash` and
/// `to_hash` (commits strictly after `from_hash`, up to and including
/// `to_hash`). `None` on shell failure.
pub fn sl_log_count(from_hash: &str, to_hash: &str) -> Option<i64> {
    let stdout = run_sl(&[
        "log",
        "--rev",
        &format!("{from_hash}::{to_hash}"),
        "-T",
        "x",
    ])?;
    Some(count_from_log_output(&stdout))
}

/// Returns the Unix epoch seconds of the commit identified by `rev`.
///
/// Mirrors PHP `BTDCachedGraphScriptControllerBase::genTimestampFromHg`,
/// which queries `last(public() & ::<rev>)`. Uses the `%s` strftime
/// template so the output is just the integer seconds (rather than
/// `{date|hgdate}` which adds the timezone offset and forces a split).
/// `None` on shell failure or if the output isn't parseable.
pub fn sl_log_timestamp(rev: &str) -> Option<i64> {
    let stdout = run_sl(&[
        "log",
        "--rev",
        &format!("last(public() & ::{rev})"),
        "-T",
        "{date(date, '%s')}",
        "--limit",
        "1",
    ])?;
    stdout.trim().parse::<i64>().ok()
}

/// `sl log -T x` emits one `x` per commit in the inclusive revset range
/// `from..=to`. We want the exclusive count (commits strictly after
/// `from`), so subtract 1. Empty stdout saturates at 0 instead of
/// underflowing.
fn count_from_log_output(stdout: &str) -> i64 {
    let inclusive = stdout.trim().chars().count();
    inclusive.saturating_sub(1) as i64
}

fn run_sl(args: &[&str]) -> Option<String> {
    let output = Command::new("sl").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_from_log_output_returns_inclusive_minus_one() {
        assert_eq!(count_from_log_output("xxxxx\n"), 4);
        assert_eq!(count_from_log_output("x"), 0);
    }

    #[test]
    fn count_from_log_output_saturates_at_zero_for_empty_stdout() {
        assert_eq!(count_from_log_output(""), 0);
        assert_eq!(count_from_log_output("\n"), 0);
    }
}
