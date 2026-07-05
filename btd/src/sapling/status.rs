/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fs;
use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;
use serde::Serialize;
use td_util_buck::types::ProjectRelativePath;
use thiserror::Error;

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub enum Status<Path> {
    Modified(Path),
    Added(Path),
    Removed(Path),
}

#[derive(Error, Debug)]
enum StatusParseError {
    #[error("Unexpected line format: {0}")]
    UnexpectedFormat(String),
    #[error("Unknown line prefix: {0}")]
    UnknownPrefix(String),
}

/// Split a status line into its leading status token and the remainder,
/// accepting either a space (`sl`/`hg`) or a tab (`git --name-status`) as the
/// separator. Returns `None` if there is no separator or the token is empty.
fn split_status_token(line: &str) -> Option<(&str, &str)> {
    let idx = line.find([' ', '\t'])?;
    if idx == 0 {
        return None;
    }
    Some((&line[..idx], &line[idx + 1..]))
}

/// Whether `token` is a git similarity-scored status like `R100` or `C75`,
/// i.e. `prefix` followed by one or more ASCII digits.
fn is_scored(token: &str, prefix: char) -> bool {
    let mut chars = token.chars();
    chars.next() == Some(prefix)
        && !token[1..].is_empty()
        && token[1..].bytes().all(|b| b.is_ascii_digit())
}

impl Status<ProjectRelativePath> {
    /// Creates a new Modified status from a file path string
    pub fn modified(path: &str) -> Self {
        Self::Modified(ProjectRelativePath::new(path))
    }

    /// Creates a new Added status from a file path string
    pub fn added(path: &str) -> Self {
        Self::Added(ProjectRelativePath::new(path))
    }

    /// Creates a new Removed status from a file path string
    pub fn removed(path: &str) -> Self {
        Self::Removed(ProjectRelativePath::new(path))
    }

    /// Parse a single-path status line (one of `sl`/`hg`'s `M path`, or
    /// git's `M\tpath`). Rename/copy lines carry two paths and must go
    /// through [`Status::parse_line`] instead.
    fn from_str(value: &str) -> anyhow::Result<Self> {
        let (typ, path) = split_status_token(value)
            .ok_or_else(|| StatusParseError::UnexpectedFormat(value.to_owned()))?;
        let path = ProjectRelativePath::new(path);
        match typ {
            "A" => Ok(Self::Added(path)),
            // `T` is git's "typechange" (e.g. file <-> symlink); treat as a
            // modification for impact purposes.
            "M" | "T" => Ok(Self::Modified(path)),
            "R" => Ok(Self::Removed(path)),
            "D" => Ok(Self::Removed(path)), // used by git and jujutsu
            _ => Err(StatusParseError::UnknownPrefix(value.to_owned()).into()),
        }
    }

    /// Parse a single status line into one or more statuses.
    ///
    /// Handles both Sapling/Mercurial `sl status` output (`M path`, space
    /// separated) and git `git diff --name-status` output (`M\tpath`, tab
    /// separated). A git rename (`R<score>\told\tnew`) expands to a
    /// [`Status::Removed`] of the old path plus a [`Status::Added`] of the new
    /// one; a copy (`C<score>\told\tnew`) yields a single [`Status::Added`] of
    /// the new path (the source is untouched). This mirrors what BTD would see
    /// had rename detection been disabled, so downstream impact analysis stays
    /// conservative.
    fn parse_line(value: &str) -> anyhow::Result<Vec<Self>> {
        let (token, rest) = split_status_token(value)
            .ok_or_else(|| StatusParseError::UnexpectedFormat(value.to_owned()))?;
        if is_scored(token, 'R') {
            let (old, new) = rest
                .split_once('\t')
                .ok_or_else(|| StatusParseError::UnexpectedFormat(value.to_owned()))?;
            return Ok(vec![
                Self::Removed(ProjectRelativePath::new(old)),
                Self::Added(ProjectRelativePath::new(new)),
            ]);
        }
        if is_scored(token, 'C') {
            let (_old, new) = rest
                .split_once('\t')
                .ok_or_else(|| StatusParseError::UnexpectedFormat(value.to_owned()))?;
            return Ok(vec![Self::Added(ProjectRelativePath::new(new))]);
        }
        Ok(vec![Self::from_str(value)?])
    }
}

impl<Path> Status<Path> {
    pub fn get(&self) -> &Path {
        match self {
            Status::Modified(x) => x,
            Status::Added(x) => x,
            Status::Removed(x) => x,
        }
    }

    pub fn map<'a, T: 'a>(&'a self, f: impl FnOnce(&'a Path) -> T) -> Status<T> {
        match self {
            Status::Modified(x) => Status::Modified(f(x)),
            Status::Added(x) => Status::Added(f(x)),
            Status::Removed(x) => Status::Removed(f(x)),
        }
    }

    pub fn try_map<T, E>(&self, f: impl FnOnce(&Path) -> Result<T, E>) -> Result<Status<T>, E> {
        Ok(match self {
            Status::Modified(x) => Status::Modified(f(x)?),
            Status::Added(x) => Status::Added(f(x)?),
            Status::Removed(x) => Status::Removed(f(x)?),
        })
    }

    pub fn into_map<T>(self, f: impl FnOnce(Path) -> T) -> Status<T> {
        match self {
            Status::Modified(x) => Status::Modified(f(x)),
            Status::Added(x) => Status::Added(f(x)),
            Status::Removed(x) => Status::Removed(f(x)),
        }
    }

    pub fn into_try_map<T, E>(self, f: impl FnOnce(Path) -> Result<T, E>) -> Result<Status<T>, E> {
        Ok(match self {
            Status::Modified(x) => Status::Modified(f(x)?),
            Status::Added(x) => Status::Added(f(x)?),
            Status::Removed(x) => Status::Removed(f(x)?),
        })
    }
}

pub fn read_status(path: &Path) -> anyhow::Result<Vec<Status<ProjectRelativePath>>> {
    parse_status(
        &fs::read_to_string(path).with_context(|| format!("When reading `{}`", path.display()))?,
    )
}

/// Parse the textual output of `sl status` / `hg status` / `git diff
/// --name-status` into a flat list of [`Status`] entries. Blank lines are
/// ignored; rename/copy lines expand to multiple entries (see
/// [`Status::parse_line`]).
pub fn parse_status(data: &str) -> anyhow::Result<Vec<Status<ProjectRelativePath>>> {
    let mut out = Vec::new();
    for line in data.lines() {
        if line.is_empty() {
            continue;
        }
        out.extend(Status::parse_line(line)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status() {
        let src = r#"
M proj/foo.rs
M bar.rs
A baz/file.txt
R quux.js
"#;
        assert_eq!(
            parse_status(&src[1..]).unwrap(),
            vec![
                Status::Modified(ProjectRelativePath::new("proj/foo.rs")),
                Status::Modified(ProjectRelativePath::new("bar.rs")),
                Status::Added(ProjectRelativePath::new("baz/file.txt")),
                Status::Removed(ProjectRelativePath::new("quux.js"))
            ]
        );
    }

    #[test]
    fn test_status_git_name_status_tab_separated() {
        let src = "M\tproj/foo.rs\nA\tbaz/file.txt\nD\tquux.js\nT\tsym.link\n";
        assert_eq!(
            parse_status(src).unwrap(),
            vec![
                Status::Modified(ProjectRelativePath::new("proj/foo.rs")),
                Status::Added(ProjectRelativePath::new("baz/file.txt")),
                Status::Removed(ProjectRelativePath::new("quux.js")),
                Status::Modified(ProjectRelativePath::new("sym.link")),
            ]
        );
    }

    #[test]
    fn test_status_git_rename_and_copy_expand() {
        let src = "R100\told/name.rs\tnew/name.rs\nC075\tsrc/a.rs\tsrc/b.rs\n";
        assert_eq!(
            parse_status(src).unwrap(),
            vec![
                Status::Removed(ProjectRelativePath::new("old/name.rs")),
                Status::Added(ProjectRelativePath::new("new/name.rs")),
                Status::Added(ProjectRelativePath::new("src/b.rs")),
            ]
        );
    }

    #[test]
    fn test_status_path_with_spaces() {
        let src = "M\tdir/with space/file.rs\n";
        assert_eq!(
            parse_status(src).unwrap(),
            vec![Status::Modified(ProjectRelativePath::new(
                "dir/with space/file.rs"
            ))]
        );
    }

    #[test]
    fn test_status_error() {
        assert!(parse_status("X quux.js").is_err());
        assert!(parse_status("notaline").is_err());
        assert!(parse_status("not a line").is_err());
        assert!(parse_status("R100\tonlyoldpath").is_err());
    }

    #[test]
    fn test_status_constructors() {
        let modified = Status::modified("foo/modified.rs");
        let modified_parsed = Status::from_str("M foo/modified.rs").unwrap();
        assert!(matches!(modified, Status::Modified(_)));
        assert_eq!(modified, modified_parsed);
        assert_eq!(modified.get().as_str(), "foo/modified.rs");

        let added = Status::added("foo/added.rs");
        let added_parsed = Status::from_str("A foo/added.rs").unwrap();
        assert!(matches!(added, Status::Added(_)));
        assert_eq!(added, added_parsed);
        assert_eq!(added.get().as_str(), "foo/added.rs");

        let removed = Status::removed("foo/removed.rs");
        let removed_parsed = Status::from_str("R foo/removed.rs").unwrap();
        assert!(matches!(removed, Status::Removed(_)));
        assert_eq!(removed, removed_parsed);
        assert_eq!(removed.get().as_str(), "foo/removed.rs");
    }
}
