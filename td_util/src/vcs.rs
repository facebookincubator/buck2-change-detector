/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Version-control abstraction so target determination works on either a
//! Sapling/Mercurial checkout or a plain git repository. Callers pick the
//! backend explicitly (e.g. via the `--vcs` flag) and use the uniform helpers,
//! which dispatch to [`crate::sapling`] or [`crate::git`]. Both backends emit
//! changesets in the `<status> <path>` shape BTD's `read_status` consumes.

use std::path::Path;
use std::path::PathBuf;

use clap::ValueEnum;

use crate::git;
use crate::sapling;

/// The version-control system backing a checkout. Also usable directly as a
/// clap argument (`--vcs sapling|sl|hg|git`), defaulting to [`Vcs::Sapling`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Vcs {
    /// Sapling or Mercurial — both driven through the `sl` CLI. The default.
    #[default]
    #[value(name = "sapling", aliases = ["sl", "hg"])]
    Sapling,
    #[value(name = "git")]
    Git,
}

impl Vcs {
    /// Synchronously compute the changeset between two revisions, in the
    /// `<status> <path>` shape BTD's `read_status`/`parse_status` consumes.
    /// Uses the backend's blocking helper so callers on a plain thread (e.g.
    /// BTD's synchronous `main`) don't need a tokio runtime.
    pub fn status(&self, base: &str, diff: &str) -> anyhow::Result<String> {
        match self {
            Vcs::Git => git::git_status(base, diff),
            Vcs::Sapling => sapling::sl_status(base, diff),
        }
    }

    /// The changeset between two revisions, in `read_status` shape.
    pub async fn status_range(
        &self,
        base: &str,
        diff: &str,
        cwd: Option<&Path>,
    ) -> anyhow::Result<String> {
        match self {
            Vcs::Git => git::status_range(base, diff, cwd).await,
            Vcs::Sapling => sapling::status_range(base, diff, cwd).await,
        }
    }

    /// The working-copy changeset (changes relative to the parent commit), in
    /// `read_status` shape.
    pub async fn status_working_copy(&self, cwd: Option<&Path>) -> anyhow::Result<String> {
        match self {
            Vcs::Git => git::status_working_copy(cwd).await,
            Vcs::Sapling => sapling::status_working_copy(cwd).await,
        }
    }

    /// The repository root.
    pub async fn repo_root(&self, cwd: Option<&Path>) -> anyhow::Result<PathBuf> {
        match self {
            Vcs::Git => git::repo_root(cwd).await,
            Vcs::Sapling => sapling::repo_root(cwd).await,
        }
    }

    /// The current commit hash.
    pub async fn current_revision(&self, cwd: Option<&Path>) -> anyhow::Result<String> {
        match self {
            Vcs::Git => git::current_revision(cwd).await,
            Vcs::Sapling => sapling::current_revision(cwd).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::ValueEnum;

    use super::*;

    #[test]
    fn value_enum_accepts_aliases() {
        // `sl`, `hg`, and `sapling` all mean the Mercurial/Sapling backend.
        assert_eq!(Vcs::from_str("sapling", true).unwrap(), Vcs::Sapling);
        assert_eq!(Vcs::from_str("sl", true).unwrap(), Vcs::Sapling);
        assert_eq!(Vcs::from_str("hg", true).unwrap(), Vcs::Sapling);
        assert_eq!(Vcs::from_str("git", true).unwrap(), Vcs::Git);
        assert!(Vcs::from_str("svn", true).is_err());
    }

    #[test]
    fn default_is_sapling() {
        assert_eq!(Vcs::default(), Vcs::Sapling);
    }
}
