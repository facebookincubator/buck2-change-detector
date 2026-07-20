/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Thin shell-out wrappers around the `git` CLI, mirroring [`crate::sapling`].
//!
//! Status helpers use `--name-status --no-renames` so renames surface as a
//! delete plus add, matching BTD's conservative impact model.

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use tempfile::NamedTempFile;
use tokio::process::Command as AsyncCommand;

const DETERMINISTIC_ARGS: [&str; 2] = ["-c", "core.quotepath=false"];

/// Run `git diff --name-status --no-renames FROM TO`.
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

/// Count commits reachable from `to_hash` but not from `from_hash`.
pub fn git_log_count(from_hash: &str, to_hash: &str) -> Option<i64> {
    let stdout = run_git(&["rev-list", "--count", &format!("{from_hash}..{to_hash}")])?;
    stdout.trim().parse::<i64>().ok()
}

/// Unix epoch seconds of the commit identified by `rev`.
pub fn git_log_timestamp(rev: &str) -> Option<i64> {
    let stdout = run_git(&["log", "-1", "--format=%ct", rev])?;
    stdout.trim().parse::<i64>().ok()
}

/// Repository root for the checkout at `cwd`.
pub async fn repo_root(cwd: Option<&Path>) -> anyhow::Result<PathBuf> {
    let stdout = run_git_async(&["rev-parse", "--show-toplevel"], cwd).await?;
    Ok(PathBuf::from(stdout.trim()))
}

/// Current commit hash for the checkout at `cwd`.
pub async fn current_revision(cwd: Option<&Path>) -> anyhow::Result<String> {
    let stdout = run_git_async(&["rev-parse", "HEAD"], cwd).await?;
    Ok(stdout.trim().to_owned())
}

/// Working-copy changes relative to `HEAD`.
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

/// Changeset between two revisions.
pub async fn status_range(base: &str, diff: &str, cwd: Option<&Path>) -> anyhow::Result<String> {
    run_git_async(&["diff", "--name-status", "--no-renames", base, diff], cwd).await
}

/// Unified git-format diff for `files` between two revisions.
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

/// Write the changeset between two revisions to a fresh `NamedTempFile`.
pub async fn changeset_tempfile(
    base: &str,
    diff: &str,
    cwd: Option<&Path>,
) -> anyhow::Result<NamedTempFile> {
    write_temp(&status_range(base, diff, cwd).await?)
}

/// Write the working-copy changeset to a fresh `NamedTempFile`.
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
        assert_eq!(DETERMINISTIC_ARGS, ["-c", "core.quotepath=false"]);
    }

    #[test]
    fn write_temp_roundtrips_contents() {
        let tmp = write_temp("M\tfoo.rs\n").unwrap();
        let read = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(read, "M\tfoo.rs\n");
    }
}
