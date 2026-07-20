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
use td_util::vcs::Vcs;
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
    #[error("Invalid git-quoted path: {0}")]
    InvalidGitQuotedPath(String),
    #[error(
        "Unmerged path in git status (`{0}`): a merge or rebase is in progress. Resolve the conflict before running BTD."
    )]
    UnmergedPath(String),
}

fn split_status_token(line: &str, separator: char) -> Option<(&str, &str)> {
    let idx = line.find(separator)?;
    if idx == 0 {
        return None;
    }
    Some((&line[..idx], &line[idx + 1..]))
}

fn is_scored(token: &str, prefix: char) -> bool {
    let mut chars = token.chars();
    chars.next() == Some(prefix)
        && !token[1..].is_empty()
        && token[1..].bytes().all(|b| b.is_ascii_digit())
}

impl Status<ProjectRelativePath> {
    pub fn modified(path: &str) -> Self {
        Self::Modified(ProjectRelativePath::new(path))
    }

    pub fn added(path: &str) -> Self {
        Self::Added(ProjectRelativePath::new(path))
    }

    pub fn removed(path: &str) -> Self {
        Self::Removed(ProjectRelativePath::new(path))
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
    read_status_with_vcs(path, Vcs::Sapling)
}

pub fn read_status_with_vcs(
    path: &Path,
    vcs: Vcs,
) -> anyhow::Result<Vec<Status<ProjectRelativePath>>> {
    parse_status_with_vcs(
        &fs::read_to_string(path).with_context(|| format!("When reading `{}`", path.display()))?,
        vcs,
    )
}

pub fn parse_status(data: &str) -> anyhow::Result<Vec<Status<ProjectRelativePath>>> {
    parse_status_with_vcs(data, Vcs::Sapling)
}

pub fn parse_status_with_vcs(
    data: &str,
    vcs: Vcs,
) -> anyhow::Result<Vec<Status<ProjectRelativePath>>> {
    let mut out = Vec::new();
    for line in data.lines() {
        if line.is_empty() {
            continue;
        }
        match vcs {
            Vcs::Sapling => out.push(parse_sapling_status_line(line)?),
            Vcs::Git => out.extend(parse_git_status_line(line)?),
        }
    }
    Ok(out)
}

fn parse_sapling_status_line(value: &str) -> anyhow::Result<Status<ProjectRelativePath>> {
    let (typ, path) = split_status_token(value, ' ')
        .ok_or_else(|| StatusParseError::UnexpectedFormat(value.to_owned()))?;
    let path = ProjectRelativePath::new(path);
    match typ {
        "A" => Ok(Status::Added(path)),
        "M" => Ok(Status::Modified(path)),
        "R" => Ok(Status::Removed(path)),
        "D" => Ok(Status::Removed(path)), // used by jujutsu
        _ => Err(StatusParseError::UnknownPrefix(value.to_owned()).into()),
    }
}

/// A Git rename (`R<score>\told\tnew`) expands to a [`Status::Removed`] of the
/// old path plus a [`Status::Added`] of the new one; a copy
/// (`C<score>\told\tnew`) yields a single [`Status::Added`] of the new path.
fn parse_git_status_line(value: &str) -> anyhow::Result<Vec<Status<ProjectRelativePath>>> {
    let (token, rest) = split_status_token(value, '\t')
        .ok_or_else(|| StatusParseError::UnexpectedFormat(value.to_owned()))?;
    if is_scored(token, 'R') {
        let (old, new) = rest
            .split_once('\t')
            .ok_or_else(|| StatusParseError::UnexpectedFormat(value.to_owned()))?;
        return Ok(vec![
            Status::Removed(ProjectRelativePath::new(&decode_git_path(old)?)),
            Status::Added(ProjectRelativePath::new(&decode_git_path(new)?)),
        ]);
    }
    if is_scored(token, 'C') {
        let (_old, new) = rest
            .split_once('\t')
            .ok_or_else(|| StatusParseError::UnexpectedFormat(value.to_owned()))?;
        return Ok(vec![Status::Added(ProjectRelativePath::new(
            &decode_git_path(new)?,
        ))]);
    }

    let path = ProjectRelativePath::new(&decode_git_path(rest)?);
    match token {
        "A" => Ok(vec![Status::Added(path)]),
        // Typechanges can affect the same dependents as content changes.
        "M" | "T" => Ok(vec![Status::Modified(path)]),
        // Unmerged paths surface in `git diff --name-status HEAD` during an
        // in-progress merge/rebase; BTD must not run against a conflicted tree.
        "U" => Err(StatusParseError::UnmergedPath(value.to_owned()).into()),
        "D" => Ok(vec![Status::Removed(path)]),
        _ => Err(StatusParseError::UnknownPrefix(value.to_owned()).into()),
    }
}

fn decode_git_path(value: &str) -> anyhow::Result<String> {
    let Some(quoted) = value.strip_prefix('"') else {
        return Ok(value.to_owned());
    };
    let quoted = quoted
        .strip_suffix('"')
        .ok_or_else(|| StatusParseError::InvalidGitQuotedPath(value.to_owned()))?;
    let mut bytes = Vec::with_capacity(quoted.len());
    let mut chars = quoted.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut buf = [0; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            continue;
        }

        let escaped = chars
            .next()
            .ok_or_else(|| StatusParseError::InvalidGitQuotedPath(value.to_owned()))?;
        match escaped {
            'a' => bytes.push(0x07),
            'b' => bytes.push(0x08),
            'f' => bytes.push(0x0c),
            'n' => bytes.push(b'\n'),
            'r' => bytes.push(b'\r'),
            't' => bytes.push(b'\t'),
            'v' => bytes.push(0x0b),
            '\\' => bytes.push(b'\\'),
            '"' => bytes.push(b'"'),
            '0'..='7' => {
                let mut octal_value = escaped
                    .to_digit(8)
                    .expect("match arm only accepts octal digits");
                for _ in 0..2 {
                    match chars.peek().and_then(|c| c.to_digit(8)) {
                        Some(digit) => {
                            chars.next();
                            octal_value = octal_value * 8 + digit;
                        }
                        None => break,
                    }
                }
                if octal_value > u8::MAX.into() {
                    return Err(StatusParseError::InvalidGitQuotedPath(value.to_string()).into());
                }
                bytes.push(octal_value as u8);
            }
            _ => return Err(StatusParseError::InvalidGitQuotedPath(value.to_owned()).into()),
        }
    }
    String::from_utf8(bytes).map_err(Into::into)
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
            parse_status_with_vcs(src, Vcs::Git).unwrap(),
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
            parse_status_with_vcs(src, Vcs::Git).unwrap(),
            vec![
                Status::Removed(ProjectRelativePath::new("old/name.rs")),
                Status::Added(ProjectRelativePath::new("new/name.rs")),
                Status::Added(ProjectRelativePath::new("src/b.rs")),
            ]
        );
    }

    #[test]
    fn test_status_git_unmerged_is_error() {
        // A conflicted (unmerged) path means a merge/rebase is in progress;
        // BTD should refuse to analyze rather than produce partial results.
        let err = parse_status_with_vcs("U\tconflicted/file.rs\n", Vcs::Git).unwrap_err();
        assert!(err.to_string().contains("merge or rebase is in progress"));
    }

    #[test]
    fn test_status_path_with_spaces() {
        let src = "M\tdir/with space/file.rs\n";
        assert_eq!(
            parse_status_with_vcs(src, Vcs::Git).unwrap(),
            vec![Status::Modified(ProjectRelativePath::new(
                "dir/with space/file.rs"
            ))]
        );
    }

    #[test]
    fn test_status_git_decodes_quoted_paths() {
        let src = "M\t\"dir/with\\ttab.rs\"\nA\t\"dir/quote\\\"and\\\\slash.rs\"\nR100\t\"old/\\303\\274.rs\"\t\"new/\\303\\274.rs\"\n";
        assert_eq!(
            parse_status_with_vcs(src, Vcs::Git).unwrap(),
            vec![
                Status::Modified(ProjectRelativePath::new("dir/with\ttab.rs")),
                Status::Added(ProjectRelativePath::new("dir/quote\"and\\slash.rs")),
                Status::Removed(ProjectRelativePath::new("old/\u{00fc}.rs")),
                Status::Added(ProjectRelativePath::new("new/\u{00fc}.rs")),
            ]
        );
    }

    #[test]
    fn test_status_error() {
        assert!(parse_status("X quux.js").is_err());
        assert!(parse_status("notaline").is_err());
        assert!(parse_status("not a line").is_err());
        assert!(parse_status_with_vcs("R100\tonlyoldpath", Vcs::Git).is_err());
        assert!(parse_status_with_vcs("M\t\"unterminated", Vcs::Git).is_err());
    }

    #[test]
    fn test_status_constructors() {
        let modified = Status::modified("foo/modified.rs");
        let modified_parsed = parse_sapling_status_line("M foo/modified.rs").unwrap();
        assert!(matches!(modified, Status::Modified(_)));
        assert_eq!(modified, modified_parsed);
        assert_eq!(modified.get().as_str(), "foo/modified.rs");

        let added = Status::added("foo/added.rs");
        let added_parsed = parse_sapling_status_line("A foo/added.rs").unwrap();
        assert!(matches!(added, Status::Added(_)));
        assert_eq!(added, added_parsed);
        assert_eq!(added.get().as_str(), "foo/added.rs");

        let removed = Status::removed("foo/removed.rs");
        let removed_parsed = parse_sapling_status_line("R foo/removed.rs").unwrap();
        assert!(matches!(removed, Status::Removed(_)));
        assert_eq!(removed, removed_parsed);
        assert_eq!(removed.get().as_str(), "foo/removed.rs");
    }
}
