/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fs;
use std::path::Path;

use anyhow::Context as _;

use crate::buck::types::ProjectRelativePath;

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum Status<Path> {
    Modified(Path),
    Added(Path),
    Removed(Path),
}

impl Status<ProjectRelativePath> {
    fn from_str(value: &str) -> anyhow::Result<Self> {
        let mut it = value.chars();
        let typ = it.next();
        if it.next() != Some(' ') {
            return Err(anyhow::anyhow!("Unexpected line format, {}", value));
        }
        let path = ProjectRelativePath::new(it.as_str());
        match typ {
            Some('A') => Ok(Self::Added(path)),
            Some('M') => Ok(Self::Modified(path)),
            Some('R') => Ok(Self::Removed(path)),
            _ => Err(anyhow::anyhow!("Unknown line prefix, {}", value)),
        }
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

    pub fn map<T>(&self, f: impl FnOnce(&Path) -> T) -> Status<T> {
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
}

pub fn read_status(path: &Path) -> anyhow::Result<Vec<Status<ProjectRelativePath>>> {
    parse_status(
        &fs::read_to_string(path).with_context(|| format!("When reading `{}`", path.display()))?,
    )
}

fn parse_status(data: &str) -> anyhow::Result<Vec<Status<ProjectRelativePath>>> {
    data.lines()
        .map(Status::from_str)
        .collect::<anyhow::Result<Vec<_>>>()
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
    fn test_status_error() {
        assert!(parse_status("X quux.js").is_err());
        assert!(parse_status("notaline").is_err());
        assert!(parse_status("not a line").is_err());
    }
}
