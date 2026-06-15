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
//! Rust client. This is the single home for SCM shell-outs across TD, so
//! callers converge on one implementation instead of re-spawning `sl`/`hg`
//! ad hoc. Sync helpers spawn via `std::process::Command`; async helpers
//! (used from `tokio` contexts) spawn via `tokio::process::Command`.

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use tempfile::NamedTempFile;
use tokio::process::Command as AsyncCommand;

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

/// The repository root (`sl root`), run in `cwd` (or the process working
/// directory when `None`). The async counterpart to
/// [`crate::project::get_repo_root`], for `tokio` callers.
pub async fn repo_root(cwd: Option<&Path>) -> anyhow::Result<PathBuf> {
    let stdout = run_sl_async(&["root"], cwd).await?;
    Ok(PathBuf::from(stdout.trim()))
}

/// The current commit hash (`sl log -r . -T {node}`), run in `cwd` (or the
/// process working directory when `None`). For devserver callers that have
/// no propagated commit input and must read it from the working copy.
pub async fn current_revision(cwd: Option<&Path>) -> anyhow::Result<String> {
    let stdout = run_sl_async(&["log", "-r", ".", "-T", "{node}"], cwd).await?;
    Ok(stdout.trim().to_owned())
}

/// Raw `sl status -amr --root-relative` for the working copy (changes
/// relative to its parent), suitable for BTD's `read_status`.
pub async fn status_working_copy(cwd: Option<&Path>) -> anyhow::Result<String> {
    run_sl_async(&["status", "-amr", "--root-relative"], cwd).await
}

/// Raw `sl status --rev BASE::DIFF -amr --root-relative` — the changeset
/// between two revisions, suitable for BTD's `read_status`.
pub async fn status_range(base: &str, diff: &str, cwd: Option<&Path>) -> anyhow::Result<String> {
    let revset = format!("{base}::{diff}");
    run_sl_async(
        &["status", "--rev", &revset, "-amr", "--root-relative"],
        cwd,
    )
    .await
}

/// Changed file paths for a single revision's changes. `rev == "."` means the
/// current commit *plus* uncommitted edits (`sl status --rev .^`); any other
/// `rev` means that commit's own change set (`sl status --change <rev>`).
pub async fn status_change(rev: &str, cwd: Option<&Path>) -> anyhow::Result<Vec<String>> {
    let stdout = if rev == "." {
        run_sl_async(&["status", "--rev", ".^", "-mar", "--root-relative"], cwd).await?
    } else {
        run_sl_async(&["status", "--change", rev, "-mar", "--root-relative"], cwd).await?
    };
    Ok(parse_changed_files(&stdout))
}

/// Unified git-format diff for `files` between two revisions
/// (`sl diff --rev BASE::DIFF --git <files>`).
pub async fn diff_git(
    base: &str,
    diff: &str,
    files: &[&str],
    cwd: Option<&Path>,
) -> anyhow::Result<String> {
    let revset = format!("{base}::{diff}");
    let mut args = vec!["diff", "--rev", revset.as_str(), "--git"];
    args.extend_from_slice(files);
    run_sl_async(&args, cwd).await
}

/// Write the changeset between two revisions to a fresh `NamedTempFile`, the
/// shape BTD consumes (`--changes`). Convenience over [`status_range`].
pub async fn changeset_tempfile(
    base: &str,
    diff: &str,
    cwd: Option<&Path>,
) -> anyhow::Result<NamedTempFile> {
    write_temp(&status_range(base, diff, cwd).await?)
}

/// Write the working-copy changeset to a fresh `NamedTempFile`.
/// Convenience over [`status_working_copy`].
pub async fn working_copy_changeset_tempfile(cwd: Option<&Path>) -> anyhow::Result<NamedTempFile> {
    write_temp(&status_working_copy(cwd).await?)
}

/// Parse `sl status` output (root-relative, e.g. `M fbcode/foo`) into changed
/// file paths, dropping the two-character `<status> ` prefix. Lines too short
/// to carry a path are skipped.
pub fn parse_changed_files(status_stdout: &str) -> Vec<String> {
    status_stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            (line.len() > 2).then(|| line[2..].to_owned())
        })
        .collect()
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

/// Run `sl <args>` with `HGPLAIN=1` for deterministic output, optionally in
/// `cwd`, returning stdout. Errors if `sl` fails to spawn or exits non-zero.
async fn run_sl_async(args: &[&str], cwd: Option<&Path>) -> anyhow::Result<String> {
    let mut cmd = AsyncCommand::new("sl");
    cmd.env("HGPLAIN", "1").args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let output = cmd.output().await?;
    if !output.status.success() {
        anyhow::bail!(
            "sl {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn write_temp(contents: &str) -> anyhow::Result<NamedTempFile> {
    let mut tmp = NamedTempFile::new()?;
    tmp.write_all(contents.as_bytes())?;
    Ok(tmp)
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

    #[test]
    fn parse_changed_files_strips_status_prefix() {
        let stdout = "M fbcode/foo.rs\nA fbcode/bar.rs\nR fbcode/baz.rs\n";
        assert_eq!(
            parse_changed_files(stdout),
            vec!["fbcode/foo.rs", "fbcode/bar.rs", "fbcode/baz.rs"],
            "each line should drop its two-char `<status> ` prefix"
        );
    }

    #[test]
    fn parse_changed_files_skips_short_and_blank_lines() {
        // Empty input, blank lines, and lines too short to carry a path
        // (<= 2 chars after trim) must not produce entries.
        assert!(parse_changed_files("").is_empty());
        assert!(parse_changed_files("\n\n").is_empty());
        assert!(
            parse_changed_files("M\n").is_empty(),
            "a bare status char with no path should be skipped"
        );
    }
}
