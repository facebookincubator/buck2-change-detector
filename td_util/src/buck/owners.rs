/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::path::Path;

use anyhow::Context as _;
use indexmap::IndexMap;
use serde::Deserialize;
use serde::Serialize;

use crate::types::ProjectRelativePath;
use crate::types::TargetLabel;

/// The output of running `buck2 uquery --json owner(...)`.
/// Maps file paths to the targets that own them.
///
/// `IndexMap` keeps buck's row order, so the artifact below is byte-stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owners(IndexMap<ProjectRelativePath, Vec<TargetLabel>>);

/// What the query concluded about one file. Buck emits an empty row for a file
/// nothing owns, so `Unowned` and `NotExamined` are different answers, and only
/// the first says anything about the build graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOwners<'a> {
    NotExamined,
    Unowned,
    Owned(&'a [TargetLabel]),
}

/// The `--changed-file-owners-output` artifact. `owners` is absent when no
/// query ran, which is not the same as nothing being owned.
#[derive(Debug, Serialize, Deserialize)]
struct ChangedFileOwners {
    schema_version: u32,
    owners: Option<Owners>,
}

const CHANGED_FILE_OWNERS_SCHEMA_VERSION: u32 = 1;

pub fn write_changed_file_owners(output_path: &Path, owners: Option<Owners>) -> anyhow::Result<()> {
    let artifact = ChangedFileOwners {
        schema_version: CHANGED_FILE_OWNERS_SCHEMA_VERSION,
        owners,
    };
    let writer = std::fs::File::create(output_path)
        .with_context(|| format!("Failed to create {}", output_path.display()))?;
    serde_json::to_writer(std::io::BufWriter::new(writer), &artifact)
        .with_context(|| format!("Failed to write {}", output_path.display()))?;
    Ok(())
}

/// `None` means no owners query ran, not that nothing is owned.
pub fn read_changed_file_owners(input_path: &Path) -> anyhow::Result<Option<Owners>> {
    let reader = std::fs::File::open(input_path)
        .with_context(|| format!("Failed to open {}", input_path.display()))?;
    let artifact: ChangedFileOwners = serde_json::from_reader(std::io::BufReader::new(reader))
        .with_context(|| format!("Failed to parse {}", input_path.display()))?;
    anyhow::ensure!(
        artifact.schema_version == CHANGED_FILE_OWNERS_SCHEMA_VERSION,
        "{} has schema_version {}, expected {}",
        input_path.display(),
        artifact.schema_version,
        CHANGED_FILE_OWNERS_SCHEMA_VERSION
    );
    Ok(artifact.owners)
}

impl Owners {
    /// Create a new Owners from a JSON string returned by Buck2
    pub fn from_json(json_str: &str) -> anyhow::Result<Self> {
        let raw_map: IndexMap<String, Vec<String>> = serde_json::from_str(json_str)?;

        let owners_map = raw_map
            .into_iter()
            .map(|(path_str, target_strs)| {
                (
                    ProjectRelativePath::new(&path_str),
                    target_strs
                        .into_iter()
                        .map(|target_str| TargetLabel::new(&target_str))
                        .collect(),
                )
            })
            .collect();

        Ok(Self(owners_map))
    }

    /// Create a new empty Owners
    pub fn new() -> Self {
        Self(IndexMap::new())
    }

    /// Create Owners from a map
    pub fn from_map(map: IndexMap<ProjectRelativePath, Vec<TargetLabel>>) -> Self {
        Self(map)
    }

    pub fn lookup(&self, path: &ProjectRelativePath) -> FileOwners<'_> {
        match self.0.get(path) {
            None => FileOwners::NotExamined,
            Some(targets) if targets.is_empty() => FileOwners::Unowned,
            Some(targets) => FileOwners::Owned(targets),
        }
    }

    /// How many files the query examined, owned or not.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Get all unique target labels across all files
    pub fn all_targets(&self) -> impl Iterator<Item = &TargetLabel> {
        self.0.values().flat_map(|targets| targets.iter())
    }
}

impl Default for Owners {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(owners: &Owners, path: &str) -> Vec<TargetLabel> {
        match owners.lookup(&ProjectRelativePath::new(path)) {
            FileOwners::Owned(targets) => targets.to_vec(),
            other => panic!("expected {path} to be owned, got {other:?}"),
        }
    }

    #[test]
    fn test_owners_from_json() {
        let json_str = r#"{
            "fbcode/target_determinator/td_util/src/buck/run.rs": [
                "fbcode//target_determinator/td_util:buck-unittest",
                "fbcode//target_determinator/td_util:buck"
            ],
            "fbcode/another/file.rs": [
                "fbcode//another:target"
            ]
        }"#;

        let owners = Owners::from_json(json_str).unwrap();

        let targets = owned(
            &owners,
            "fbcode/target_determinator/td_util/src/buck/run.rs",
        );
        assert_eq!(targets.len(), 2);
        assert!(targets.contains(&TargetLabel::new(
            "fbcode//target_determinator/td_util:buck-unittest"
        )));
        assert!(targets.contains(&TargetLabel::new(
            "fbcode//target_determinator/td_util:buck"
        )));

        let another_targets = owned(&owners, "fbcode/another/file.rs");
        assert_eq!(another_targets.len(), 1);
        assert!(another_targets.contains(&TargetLabel::new("fbcode//another:target")));

        // Test all_targets iterator
        let all_targets: Vec<_> = owners.all_targets().collect();
        assert_eq!(all_targets.len(), 3);
    }

    #[test]
    fn test_unowned_file_is_distinct_from_unexamined_file() {
        let json_str = r#"{
            "fbcode/another/file.rs": ["fbcode//another:target"],
            "fbcode/another/README.md": []
        }"#;

        let owners = Owners::from_json(json_str).unwrap();

        assert_eq!(
            owners.lookup(&ProjectRelativePath::new("fbcode/another/README.md")),
            FileOwners::Unowned,
            "an empty row means the query asked and nothing owns the file"
        );
        assert_eq!(
            owners.lookup(&ProjectRelativePath::new("xplat/never/queried.cpp")),
            FileOwners::NotExamined,
            "a missing row means the query never asked, which claims nothing"
        );
        assert_eq!(
            owners.all_targets().count(),
            1,
            "an unowned row contributes no targets"
        );
    }

    #[test]
    fn test_owners_from_map() {
        let mut map = IndexMap::new();
        map.insert(
            ProjectRelativePath::new("test/file.rs"),
            vec![TargetLabel::new("test//target:name")],
        );

        let owners = Owners::from_map(map);

        let targets = owned(&owners, "test/file.rs");
        assert_eq!(targets.len(), 1);
        assert!(targets.contains(&TargetLabel::new("test//target:name")));
    }

    #[test]
    fn test_owners_new() {
        let owners = Owners::new();
        assert_eq!(
            owners.lookup(&ProjectRelativePath::new("nonexistent.rs")),
            FileOwners::NotExamined,
            "an empty map has examined nothing, so it judges nothing"
        );

        let all_targets: Vec<_> = owners.all_targets().collect();
        assert!(all_targets.is_empty());
    }

    #[test]
    fn test_owners_serialize_in_the_order_buck_emitted() {
        let json_str = r#"{
            "z/last.rs": ["fbcode//z:target"],
            "a/first.rs": [],
            "m/middle.rs": ["fbcode//m:target"]
        }"#;

        let owners = Owners::from_json(json_str).expect("parse owners");

        assert_eq!(
            serde_json::to_string(&owners).expect("serialize owners"),
            r#"{"z/last.rs":["fbcode//z:target"],"a/first.rs":[],"m/middle.rs":["fbcode//m:target"]}"#,
            "rows keep buck's order, so the artifact is byte-stable"
        );
    }

    #[test]
    fn test_changed_file_owners_round_trip() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("changed_file_owners.json");
        let owners = Owners::from_json(r#"{"a.rs": ["fbcode//a:target"], "b.md": []}"#)
            .expect("parse owners");

        write_changed_file_owners(&path, Some(owners.clone())).expect("write owners");

        assert_eq!(
            read_changed_file_owners(&path).expect("read owners"),
            Some(owners),
            "the unowned row survives the round trip, not just the owned one"
        );
    }

    #[test]
    fn test_changed_file_owners_keeps_no_query_distinct_from_empty_query() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let no_query = dir.path().join("no_query.json");
        let empty_query = dir.path().join("empty_query.json");

        write_changed_file_owners(&no_query, None).expect("write no query");
        write_changed_file_owners(&empty_query, Some(Owners::new())).expect("write empty query");

        assert_eq!(
            read_changed_file_owners(&no_query).expect("read no query"),
            None,
            "no query ran, so the artifact claims nothing about any file"
        );
        assert_eq!(
            read_changed_file_owners(&empty_query).expect("read empty query"),
            Some(Owners::new()),
            "a query that examined no files is still a query that ran"
        );
    }

    #[test]
    fn test_changed_file_owners_rejects_unknown_schema_version() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("changed_file_owners.json");
        std::fs::write(&path, r#"{"schema_version": 2, "owners": {}}"#).expect("write artifact");

        assert!(
            read_changed_file_owners(&path).is_err(),
            "a version this binary does not understand must not be read as an empty map"
        );
    }
}
