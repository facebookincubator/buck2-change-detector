/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Thin shell-out wrappers around the `git` CLI, mirroring [`crate::sapling`]
//! so callers on a plain git repository get the same read-only status / log
//! queries without a native git library. Output is shaped to be consumed by
//! BTD's `read_status` (`<status> <path>` lines, one per change).
//!
//! Two deliberate choices keep git output faithful to what BTD expects:
//!
//! * `--name-status --no-renames` — a rename surfaces as a delete of the old
//!   path plus an add of the new one, which is the conservative signal target
//!   determination wants (both packages are rebuilt) and avoids git's rename
//!   detection, which is O(files) and slow on very large diffs.
//! * `-c core.quotepath=false` — non-ASCII paths are emitted literally instead
//!   of octal-escaped, so paths round-trip unchanged (the moral equivalent of
//!   Sapling's `HGPLAIN=1`).
//!
//! Sync helpers spawn via `std::process::Command`; async helpers (used from
//! `tokio` contexts) spawn via `tokio::process::Command`.

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use tempfile::NamedTempFile;
use tokio::process::Command as AsyncCommand;

/// Config args applied to every `git` invocation for deterministic,
/// machine-parseable output regardless of the user's git config.
const DETERMINISTIC_ARGS: [&str; 2] = ["-c", "core.quotepath=false"];

/// Run `git diff --name-status --no-renames FROM TO`, returning the raw
/// stdout. Compares the two revisions directly (endpoint to endpoint, like
/// `git diff FROM..TO`), matching `sl status --rev FROM --rev TO`. Errors if
/// `git` fails to spawn or exits non-zero.
pub fn git_status(from_hash: &str, to_hash: &str) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(DETERMINISTIC_ARGS)
        .args(["diff", "--name-status", "--no-renames", from_hash, to_hash])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git diff --name-status {from_hash} {to_hash} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Number of commits reachable from `to_hash` but not from `from_hash`
/// (`git rev-list --count FROM..TO`), i.e. the exclusive commit distance.
/// `None` on shell failure or unparseable output.
pub fn git_log_count(from_hash: &str, to_hash: &str) -> Option<i64> {
    let stdout = run_git(&["rev-list", "--count", &format!("{from_hash}..{to_hash}")])?;
    stdout.trim().parse::<i64>().ok()
}

/// Unix epoch seconds of the commit identified by `rev`
/// (`git log -1 --format=%ct REV`). Mirrors [`crate::sapling::sl_log_timestamp`].
/// `None` on shell failure or if the output isn't parseable.
pub fn git_log_timestamp(rev: &str) -> Option<i64> {
    let stdout = run_git(&["log", "-1", "--format=%ct", rev])?;
    stdout.trim().parse::<i64>().ok()
}

/// The repository root (`git rev-parse --show-toplevel`), run in `cwd` (or the
/// process working directory when `None`). The async counterpart to the
/// Sapling [`crate::sapling::repo_root`].
pub async fn repo_root(cwd: Option<&Path>) -> anyhow::Result<PathBuf> {
    let stdout = run_git_async(&["rev-parse", "--show-toplevel"], cwd).await?;
    Ok(PathBuf::from(stdout.trim()))
}

/// The current commit hash (`git rev-parse HEAD`), run in `cwd` (or the
/// process working directory when `None`).
pub async fn current_revision(cwd: Option<&Path>) -> anyhow::Result<String> {
    let stdout = run_git_async(&["rev-parse", "HEAD"], cwd).await?;
    Ok(stdout.trim().to_owned())
}

/// Working-copy changes relative to `HEAD` (staged and unstaged), plus
/// untracked files reported as additions, suitable for BTD's `read_status`.
/// Mirrors Sapling's `sl status -amr`.
pub async fn status_working_copy(cwd: Option<&Path>) -> anyhow::Result<String> {
    let mut out = run_git_async(&["diff", "--name-status", "--no-renames", "HEAD"], cwd).await?;
    // Untracked files are not part of `git diff`; list them explicitly and
    // present each as an addition so new sources register as changes.
    let untracked = run_git_async(&["ls-files", "--others", "--exclude-standard"], cwd).await?;
    for path in untracked.lines() {
        if !path.is_empty() {
            out.push('A');
            out.push('\t');
            out.push_str(path);
            out.push('\n');
        }
    }
    Ok(out)
}

/// The changeset between two revisions (`git diff --name-status --no-renames
/// BASE DIFF`), suitable for BTD's `read_status`. Mirrors Sapling's
/// `status_range`.
pub async fn status_range(base: &str, diff: &str, cwd: Option<&Path>) -> anyhow::Result<String> {
    run_git_async(
        &["diff", "--name-status", "--no-renames", base, diff],
        cwd,
    )
    .await
}

/// Unified git-format diff for `files` between two revisions
/// (`git diff BASE DIFF -- <files>`). Native git output; no reformatting.
pub async fn diff_git(
    base: &str,
    diff: &str,
    files: &[&str],
    cwd: Option<&Path>,
) -> anyhow::Result<String> {
    let mut args = vec!["diff", base, diff, "--"];
    args.extend_from_slice(files);
    run_git_async(&args, cwd).await
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

fn run_git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(DETERMINISTIC_ARGS)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Run `git <args>` with deterministic config, optionally in `cwd`, returning
/// stdout. Errors if `git` fails to spawn or exits non-zero.
async fn run_git_async(args: &[&str], cwd: Option<&Path>) -> anyhow::Result<String> {
    let mut cmd = AsyncCommand::new("git");
    cmd.args(DETERMINISTIC_ARGS).args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let output = cmd.output().await?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed:\n{}",
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
    fn deterministic_args_disable_quotepath() {
        // The literal-path flag must be present so non-ASCII paths round-trip.
        assert_eq!(DETERMINISTIC_ARGS, ["-c", "core.quotepath=false"]);
    }

    #[test]
    fn write_temp_roundtrips_contents() {
        let tmp = write_temp("M\tfoo.rs\n").unwrap();
        let read = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(read, "M\tfoo.rs\n");
    }
}
