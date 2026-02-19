/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::hash::Hasher;
use std::str::FromStr;

use dashmap::DashMap;
use dashmap::DashSet;
use fnv::FnvHasher;
use serde::Deserialize;
use serde::Serialize;

use crate::types::Package;
use crate::types::PatternType;
use crate::types::TargetHash;
use crate::types::TargetPattern;

pub const CI_HINT_RULE_TYPE: &str = "ci_hint";

/// Schema version for TargetGraph serialization format.
/// Increment this when making breaking changes to TargetGraph or MinimizedBuckTarget structs.
pub const SCHEMA_VERSION: u32 = 3;

macro_rules! impl_string_storage {
    ($id_type:ident, $store_method:ident, $get_string_method:ident, $len_method:ident, $iter_method:ident, $map_field:ident) => {
        // NOTE: We use entry().or_insert_with() instead of insert() for performance.
        // This is safe because the ID is a hash of the string (see define_id_type! macro),
        // so the same string always produces the same ID. Since we're storing the string
        // as the value, inserting once vs. overwriting produces identical results.
        // This optimization avoids redundant String allocations and write locks when
        // the same key is stored multiple times (common during graph construction).
        pub fn $store_method(&self, s: &str) -> $id_type {
            let id = s.parse().unwrap();
            self.$map_field.entry(id).or_insert_with(|| s.to_string());
            id
        }

        pub fn $get_string_method(&self, id: $id_type) -> Option<String> {
            self.$map_field.get(&id).map(|v| v.clone())
        }

        pub fn $len_method(&self) -> usize {
            self.$map_field.len()
        }

        pub fn $iter_method(&self) -> impl Iterator<Item = ($id_type, String)> + '_ {
            self.$map_field
                .iter()
                .map(|entry| (*entry.key(), entry.value().clone()))
        }
    };
}

macro_rules! impl_collection_storage {
    ($key_type:ident, $value_type:ident, $store_method:ident, $add_method:ident, $get_method:ident, $len_method:ident, $iter_method:ident, $map_field:ident) => {
        pub fn $store_method(&self, key: $key_type, values: Vec<$value_type>) {
            if !values.is_empty() {
                self.$map_field.insert(key, values);
            }
        }

        pub fn $add_method(&self, key: $key_type, value: $value_type) {
            self.$map_field.entry(key).or_default().push(value);
        }

        pub fn $get_method(&self, key: $key_type) -> Option<Vec<$value_type>> {
            self.$map_field.get(&key).map(|v| v.clone())
        }

        pub fn $len_method(&self) -> usize {
            self.$map_field.len()
        }

        pub fn $iter_method(&self) -> impl Iterator<Item = ($key_type, Vec<$value_type>)> + '_ {
            self.$map_field
                .iter()
                .map(|entry| (*entry.key(), entry.value().clone()))
        }
    };
}

macro_rules! define_id_type {
    ($name:ident) => {
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            Hash,
            PartialOrd,
            Ord,
            Serialize,
            Deserialize
        )]
        pub struct $name(u64);

        impl $name {
            pub fn as_u64(&self) -> u64 {
                self.0
            }
        }

        impl FromStr for $name {
            type Err = std::convert::Infallible;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let mut hasher = FnvHasher::default();
                hasher.write(s.as_bytes());
                Ok(Self(hasher.finish()))
            }
        }
    };
}

define_id_type!(TargetId);
define_id_type!(RuleTypeId);
define_id_type!(OncallId);
define_id_type!(LabelId);
define_id_type!(GlobPatternId);
define_id_type!(FileId);
define_id_type!(PackageId);
define_id_type!(CiDepsPatternId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinimizedBuckTarget {
    pub rule_type: RuleTypeId,
    pub oncall: Option<OncallId>,
    pub labels: Vec<LabelId>,
    pub target_hash: TargetHash,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TargetGraph {
    // We store BuckTargets as ids as a form of string interning
    // These maps are used to convert Ids back to strings
    target_id_to_label: DashMap<TargetId, String>,
    rule_type_id_to_string: DashMap<RuleTypeId, String>,
    oncall_id_to_string: DashMap<OncallId, String>,
    label_id_to_string: DashMap<LabelId, String>,
    minimized_targets: DashMap<TargetId, MinimizedBuckTarget>,
    glob_pattern_id_to_string: DashMap<GlobPatternId, String>,
    package_id_to_path: DashMap<PackageId, String>,
    file_id_to_path: DashMap<FileId, String>,
    ci_deps_pattern_id_to_string: DashMap<CiDepsPatternId, String>,

    // Bidirectional dependency tracking
    target_id_to_rdeps: DashMap<TargetId, Vec<TargetId>>,
    target_id_to_deps: DashMap<TargetId, Vec<TargetId>>,

    // File relationship tracking for BZL imports
    file_id_to_rdeps: DashMap<FileId, Vec<FileId>>,

    // Package error tracking
    package_id_to_errors: DashMap<PackageId, Vec<String>>,

    // Package to targets mapping
    package_id_to_targets: DashMap<PackageId, Vec<TargetId>>,

    // CI pattern storage
    target_id_to_ci_srcs: DashMap<TargetId, Vec<GlobPatternId>>,
    target_id_to_ci_srcs_must_match: DashMap<TargetId, Vec<GlobPatternId>>,

    // CI deps patterns storage
    target_id_to_ci_deps_package_patterns: DashMap<TargetId, Vec<CiDepsPatternId>>,
    target_id_to_ci_deps_recursive_patterns: DashMap<TargetId, Vec<CiDepsPatternId>>,

    // Targets that have the uses_sudo label
    targets_with_sudo_label: DashSet<TargetId>,

    // CI hint edge storage (separate from regular deps/rdeps)
    // ci_hint → targets it affects (when ci_hint changes, these targets are impacted)
    ci_hint_to_affected: DashMap<TargetId, Vec<TargetId>>,
    // target → CI hints that affect it (reverse lookup for cleanup)
    affected_to_ci_hints: DashMap<TargetId, Vec<TargetId>>,
}

impl TargetGraph {
    pub fn new() -> Self {
        Self {
            target_id_to_label: DashMap::new(),
            rule_type_id_to_string: DashMap::new(),
            oncall_id_to_string: DashMap::new(),
            label_id_to_string: DashMap::new(),
            minimized_targets: DashMap::new(),
            glob_pattern_id_to_string: DashMap::new(),
            target_id_to_rdeps: DashMap::new(),
            target_id_to_deps: DashMap::new(),
            file_id_to_path: DashMap::new(),
            file_id_to_rdeps: DashMap::new(),
            package_id_to_path: DashMap::new(),
            package_id_to_errors: DashMap::new(),
            package_id_to_targets: DashMap::new(),
            ci_deps_pattern_id_to_string: DashMap::new(),
            target_id_to_ci_srcs: DashMap::new(),
            target_id_to_ci_srcs_must_match: DashMap::new(),
            target_id_to_ci_deps_package_patterns: DashMap::new(),
            target_id_to_ci_deps_recursive_patterns: DashMap::new(),
            targets_with_sudo_label: DashSet::new(),
            ci_hint_to_affected: DashMap::new(),
            affected_to_ci_hints: DashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.minimized_targets_len()
    }

    pub fn is_empty(&self) -> bool {
        self.minimized_targets_len() == 0
    }

    impl_string_storage!(
        TargetId,
        store_target,
        get_target_label,
        targets_len,
        iter_targets,
        target_id_to_label
    );

    impl_string_storage!(
        RuleTypeId,
        store_rule_type,
        get_rule_type_string,
        rule_types_len,
        iter_rule_types,
        rule_type_id_to_string
    );

    impl_string_storage!(
        OncallId,
        store_oncall,
        get_oncall_string,
        oncalls_len,
        iter_oncalls,
        oncall_id_to_string
    );

    impl_string_storage!(
        LabelId,
        store_label,
        get_label_string,
        labels_len,
        iter_labels,
        label_id_to_string
    );

    impl_string_storage!(
        GlobPatternId,
        store_glob_pattern,
        get_glob_pattern_string,
        glob_patterns_len,
        iter_glob_patterns,
        glob_pattern_id_to_string
    );

    impl_string_storage!(
        FileId,
        store_file,
        get_file_path,
        files_len,
        iter_files,
        file_id_to_path
    );

    impl_string_storage!(
        PackageId,
        store_package,
        get_package_path,
        packages_len,
        iter_packages,
        package_id_to_path
    );

    impl_string_storage!(
        CiDepsPatternId,
        store_ci_deps_pattern,
        get_ci_deps_pattern_string,
        ci_deps_patterns_len,
        iter_ci_deps_patterns,
        ci_deps_pattern_id_to_string
    );

    impl_collection_storage!(
        TargetId,
        GlobPatternId,
        store_ci_srcs,
        add_ci_src,
        get_ci_srcs,
        ci_srcs_len,
        iter_ci_srcs,
        target_id_to_ci_srcs
    );
    impl_collection_storage!(
        TargetId,
        GlobPatternId,
        store_ci_srcs_must_match,
        add_ci_src_must_match,
        get_ci_srcs_must_match,
        ci_srcs_must_match_len,
        iter_ci_srcs_must_match,
        target_id_to_ci_srcs_must_match
    );

    impl_collection_storage!(
        PackageId,
        String,
        store_errors,
        add_error,
        get_errors,
        errors_len,
        iter_packages_with_errors,
        package_id_to_errors
    );

    impl_collection_storage!(
        TargetId,
        CiDepsPatternId,
        store_ci_deps_package_patterns,
        add_ci_deps_package_pattern,
        get_ci_deps_package_patterns,
        ci_deps_package_patterns_len,
        iter_ci_deps_package_patterns,
        target_id_to_ci_deps_package_patterns
    );
    impl_collection_storage!(
        TargetId,
        CiDepsPatternId,
        store_ci_deps_recursive_patterns,
        add_ci_deps_recursive_pattern,
        get_ci_deps_recursive_patterns,
        ci_deps_recursive_patterns_len,
        iter_ci_deps_recursive_patterns,
        target_id_to_ci_deps_recursive_patterns
    );

    // Bidirectional dependencies storage - always maintains both directions
    pub fn add_rdep(&self, target_id: TargetId, dependent_target: TargetId) {
        // Note: We intentionally don't check for duplicate existence for performance reasons.
        // Store reverse dependency: dependent_target depends on target_id
        self.target_id_to_rdeps
            .entry(target_id)
            .or_default()
            .push(dependent_target);

        // Also store forward dependency: dependent_target -> target_id
        self.target_id_to_deps
            .entry(dependent_target)
            .or_default()
            .push(target_id);
    }

    pub fn remove_rdep(&self, target_id: TargetId, dependent_target: TargetId) {
        // Remove from reverse dependencies
        if let Some(mut rdeps) = self.target_id_to_rdeps.get_mut(&target_id) {
            rdeps.retain(|&id| id != dependent_target);
            if rdeps.is_empty() {
                drop(rdeps);
                self.target_id_to_rdeps.remove(&target_id);
            }
        }

        // Remove from forward dependencies
        if let Some(mut deps) = self.target_id_to_deps.get_mut(&dependent_target) {
            deps.retain(|&id| id != target_id);
            if deps.is_empty() {
                drop(deps);
                self.target_id_to_deps.remove(&dependent_target);
            }
        }
    }

    pub fn get_rdeps(&self, target_id: TargetId) -> Option<Vec<TargetId>> {
        self.target_id_to_rdeps.get(&target_id).map(|v| v.clone())
    }

    pub fn get_deps(&self, target_id: TargetId) -> Option<Vec<TargetId>> {
        self.target_id_to_deps.get(&target_id).map(|v| v.clone())
    }

    fn remove_from_rdeps(&self, dep_id: TargetId, target_to_remove: TargetId) {
        if let Some(mut rdeps) = self.target_id_to_rdeps.get_mut(&dep_id) {
            rdeps.retain(|&id| id != target_to_remove);
            if rdeps.is_empty() {
                drop(rdeps);
                self.target_id_to_rdeps.remove(&dep_id);
            }
        }
    }

    fn remove_from_deps(&self, target_id: TargetId, dep_to_remove: TargetId) {
        if let Some(mut deps) = self.target_id_to_deps.get_mut(&target_id) {
            deps.retain(|&id| id != dep_to_remove);
            if deps.is_empty() {
                drop(deps);
                self.target_id_to_deps.remove(&target_id);
            }
        }
    }

    pub fn remove_target(&self, target_id: TargetId) {
        if self.is_ci_hint_target(target_id) {
            self.remove_ci_hint_target(target_id);
        } else {
            self.remove_regular_target(target_id);
        }

        self.target_id_to_deps.remove(&target_id);
        self.target_id_to_ci_srcs.remove(&target_id);
        self.target_id_to_ci_srcs_must_match.remove(&target_id);
        self.target_id_to_ci_deps_package_patterns
            .remove(&target_id);
        self.target_id_to_ci_deps_recursive_patterns
            .remove(&target_id);
        self.targets_with_sudo_label.remove(&target_id);
        self.minimized_targets.remove(&target_id);
    }

    fn remove_ci_hint_target(&self, target_id: TargetId) {
        if let Some(rdeps) = self.get_rdeps(target_id) {
            for dest_id in rdeps {
                self.remove_from_deps(dest_id, target_id);
            }
        }
        self.target_id_to_rdeps.remove(&target_id);

        if let Some(deps) = self.get_deps(target_id) {
            for dep_id in deps {
                self.remove_from_rdeps(dep_id, target_id);
            }
        }
    }

    fn remove_regular_target(&self, target_id: TargetId) {
        if let Some(deps) = self.get_deps(target_id) {
            for dep_id in deps {
                if !self.is_ci_hint_target(dep_id) {
                    self.remove_from_rdeps(dep_id, target_id);
                }
            }
        }
    }

    pub fn get_all_targets(&self) -> impl Iterator<Item = TargetId> + '_ {
        self.target_id_to_label.iter().map(|entry| *entry.key())
    }

    pub fn store_minimized_target(&self, target_id: TargetId, target: MinimizedBuckTarget) {
        self.minimized_targets.insert(target_id, target);
    }

    pub fn get_minimized_target(&self, id: TargetId) -> Option<MinimizedBuckTarget> {
        self.minimized_targets.get(&id).map(|entry| entry.clone())
    }

    pub fn is_ci_hint_target(&self, target_id: TargetId) -> bool {
        self.get_minimized_target(target_id)
            .and_then(|minimized| self.get_rule_type_string(minimized.rule_type))
            .is_some_and(|rule_type| rule_type == CI_HINT_RULE_TYPE)
    }

    pub fn mark_target_has_sudo_label(&self, target_id: TargetId) {
        self.targets_with_sudo_label.insert(target_id);
    }

    pub fn has_sudo_label(&self, target_id: TargetId) -> bool {
        self.targets_with_sudo_label.contains(&target_id)
    }

    pub fn iter_targets_with_sudo_label(&self) -> impl Iterator<Item = TargetId> + '_ {
        self.targets_with_sudo_label.iter().map(|entry| *entry)
    }

    pub fn targets_with_sudo_label_len(&self) -> usize {
        self.targets_with_sudo_label.len()
    }

    // Size analysis methods
    pub fn rdeps_len(&self) -> usize {
        self.target_id_to_rdeps.len()
    }

    pub fn deps_len(&self) -> usize {
        self.target_id_to_deps.len()
    }

    // File reverse dependencies storage - similar to target rdeps
    pub fn add_file_rdep(&self, file_id: FileId, dependent_file: FileId) {
        // Note: We intentionally don't check for duplicate existence for performance reasons.
        self.file_id_to_rdeps
            .entry(file_id)
            .or_default()
            .push(dependent_file);
    }

    pub fn get_file_rdeps(&self, file_id: FileId) -> Option<Vec<FileId>> {
        self.file_id_to_rdeps.get(&file_id).map(|v| v.clone())
    }

    pub fn file_rdeps_len(&self) -> usize {
        self.file_id_to_rdeps.len()
    }

    pub fn minimized_targets_len(&self) -> usize {
        self.minimized_targets.len()
    }

    /// Display analysis of internal data structures
    pub fn print_size_analysis(&self) {
        // Create a vector of tuples (name, size) for all storage collections
        let sizes = vec![
            ("targets", self.targets_len()),
            ("rdeps", self.rdeps_len()),
            ("deps", self.deps_len()),
            ("rule_types", self.rule_types_len()),
            ("oncalls", self.oncalls_len()),
            ("labels", self.labels_len()),
            ("minimized_targets", self.minimized_targets_len()),
            ("glob_patterns", self.glob_patterns_len()),
            ("files", self.files_len()),
            ("file_rdeps", self.file_rdeps_len()),
            ("packages", self.packages_len()),
            ("ci_deps_patterns", self.ci_deps_patterns_len()),
            ("errors", self.errors_len()),
            ("ci_srcs", self.ci_srcs_len()),
            ("ci_srcs_must_match", self.ci_srcs_must_match_len()),
            (
                "ci_deps_package_patterns",
                self.ci_deps_package_patterns_len(),
            ),
            (
                "ci_deps_recursive_patterns",
                self.ci_deps_recursive_patterns_len(),
            ),
            (
                "targets_with_sudo_label",
                self.targets_with_sudo_label_len(),
            ),
            ("ci_hint_to_affected", self.ci_hint_to_affected_len()),
            ("affected_to_ci_hints", self.affected_to_ci_hints_len()),
        ];

        tracing::info!("TargetGraph DashMap sizes:");
        for (name, size) in sizes {
            tracing::info!("  {}: {}", name, size);
        }
    }

    pub fn ci_deps_pattern_id_to_target_pattern(
        &self,
        pattern_id: CiDepsPatternId,
        pattern_type: PatternType,
    ) -> Option<TargetPattern> {
        self.get_ci_deps_pattern_string(pattern_id)
            .map(|pattern_string| Package::new(&pattern_string).to_target_pattern(pattern_type))
    }

    pub fn add_target_to_package(&self, package_id: PackageId, target_id: TargetId) {
        self.package_id_to_targets
            .entry(package_id)
            .or_default()
            .push(target_id);
    }

    pub fn get_targets_in_package(&self, package_id: PackageId) -> Option<Vec<TargetId>> {
        self.package_id_to_targets
            .get(&package_id)
            .map(|v| v.clone())
    }

    pub fn add_ci_hint_edge(&self, ci_hint_id: TargetId, affected_target: TargetId) {
        self.ci_hint_to_affected
            .entry(ci_hint_id)
            .or_default()
            .push(affected_target);
        self.affected_to_ci_hints
            .entry(affected_target)
            .or_default()
            .push(ci_hint_id);
    }

    pub fn remove_ci_hint_edge(&self, ci_hint_id: TargetId, affected_target: TargetId) {
        if let Some(mut affected) = self.ci_hint_to_affected.get_mut(&ci_hint_id) {
            affected.retain(|&id| id != affected_target);
            if affected.is_empty() {
                drop(affected);
                self.ci_hint_to_affected.remove(&ci_hint_id);
            }
        }
        if let Some(mut ci_hints) = self.affected_to_ci_hints.get_mut(&affected_target) {
            ci_hints.retain(|&id| id != ci_hint_id);
            if ci_hints.is_empty() {
                drop(ci_hints);
                self.affected_to_ci_hints.remove(&affected_target);
            }
        }
    }

    pub fn get_ci_hint_affected(&self, ci_hint_id: TargetId) -> Option<Vec<TargetId>> {
        self.ci_hint_to_affected.get(&ci_hint_id).map(|v| v.clone())
    }

    pub fn get_affecting_ci_hints(&self, target_id: TargetId) -> Option<Vec<TargetId>> {
        self.affected_to_ci_hints.get(&target_id).map(|v| v.clone())
    }

    pub fn ci_hint_to_affected_len(&self) -> usize {
        self.ci_hint_to_affected.len()
    }

    pub fn affected_to_ci_hints_len(&self) -> usize {
        self.affected_to_ci_hints.len()
    }
}

impl Default for TargetGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[test]
    fn test_target_id_creation() {
        let target_label = "fbcode//buck2:buck2";
        let id1 = target_label.parse::<TargetId>().unwrap();
        let id2 = target_label.parse::<TargetId>().unwrap();

        // Same string should produce same TargetId
        assert_eq!(id1, id2);

        // Different strings should produce different TargetIds
        let id3 = "fbcode//other:target".parse::<TargetId>().unwrap();
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_target_graph_basic_operations() {
        let graph = TargetGraph::new();

        let target1 = "fbcode//a:target1";
        let target2 = "fbcode//b:target2";

        let id1 = graph.store_target(target1);
        let id2 = graph.store_target(target2);

        assert_ne!(id1, id2);
        assert_eq!(graph.targets_len(), 2);

        let id1_again = graph.store_target(target1);
        assert_eq!(id1, id1_again);
    }

    #[test]
    fn test_reverse_dependencies() {
        let graph = TargetGraph::new();

        let target1 = "fbcode//a:target1";
        let target2 = "fbcode//b:target2";
        let target3 = "fbcode//c:target3";

        let id1 = graph.store_target(target1);
        let id2 = graph.store_target(target2);
        let id3 = graph.store_target(target3);

        graph.add_rdep(id1, id2);
        graph.add_rdep(id1, id3);

        let rdeps = graph.get_rdeps(id1).unwrap();
        assert_eq!(rdeps.len(), 2);
        assert!(rdeps.contains(&id2));
        assert!(rdeps.contains(&id3));

        assert!(graph.get_rdeps(id2).is_none());
        assert!(graph.get_rdeps(id3).is_none());
    }

    #[test]
    fn test_serialization() {
        let graph = TargetGraph::new();

        let target1 = "fbcode//a:target1";
        let target2 = "fbcode//b:target2";
        let target3 = "fbcode//c:target3";

        let id1 = graph.store_target(target1);
        let id2 = graph.store_target(target2);
        let id3 = graph.store_target(target3);

        graph.add_rdep(id1, id2);
        graph.add_rdep(id1, id3);

        let json = serde_json::to_string(&graph).expect("Failed to serialize");
        let restored_graph: TargetGraph =
            serde_json::from_str(&json).expect("Failed to deserialize");

        assert_eq!(restored_graph.targets_len(), 3);

        let restored_rdeps1 = restored_graph.get_rdeps(id1).unwrap();
        assert_eq!(restored_rdeps1.len(), 2);
        assert!(restored_rdeps1.contains(&id2));
        assert!(restored_rdeps1.contains(&id3));

        assert_eq!(restored_graph.store_target(target1), id1);
        assert_eq!(restored_graph.store_target(target2), id2);
        assert_eq!(restored_graph.store_target(target3), id3);
    }

    #[test]
    fn test_new_id_types() {
        // Test TargetId
        let target1 = "fbcode//a:target1";
        let target2 = "fbcode//b:target2";
        let target_id1: TargetId = target1.parse().unwrap();
        let target_id2: TargetId = target2.parse().unwrap();
        assert_ne!(target_id1, target_id2);
        assert_eq!(target1.parse::<TargetId>().unwrap(), target_id1);

        // Test RuleTypeId
        let rule1 = "cpp_library";
        let rule2 = "python_library";
        let rule_id1: RuleTypeId = rule1.parse().unwrap();
        let rule_id2: RuleTypeId = rule2.parse().unwrap();
        assert_ne!(rule_id1, rule_id2);
        assert_eq!(rule1.parse::<RuleTypeId>().unwrap(), rule_id1);

        // Test OncallId
        let oncall1 = "team_a";
        let oncall2 = "team_b";
        let oncall_id1: OncallId = oncall1.parse().unwrap();
        let oncall_id2: OncallId = oncall2.parse().unwrap();
        assert_ne!(oncall_id1, oncall_id2);
        assert_eq!(oncall1.parse::<OncallId>().unwrap(), oncall_id1);

        // Test LabelId
        let label1 = "ci_test";
        let label2 = "production";
        let label_id1: LabelId = label1.parse().unwrap();
        let label_id2: LabelId = label2.parse().unwrap();
        assert_ne!(label_id1, label_id2);
        assert_eq!(label1.parse::<LabelId>().unwrap(), label_id1);
    }

    #[test]
    fn test_string_storage_and_retrieval() {
        let graph = TargetGraph::new();

        // Store and retrieve target
        let target_label = "fbcode//test:target";
        let target_id = graph.store_target(target_label);
        assert_eq!(
            graph.get_target_label(target_id),
            Some(target_label.to_string())
        );

        // Store and retrieve rule type
        let rule_type = "cpp_library";
        let rule_id = graph.store_rule_type(rule_type);
        assert_eq!(
            graph.get_rule_type_string(rule_id),
            Some(rule_type.to_string())
        );

        // Store and retrieve oncall
        let oncall = "team_efficiency";
        let oncall_id = graph.store_oncall(oncall);
        assert_eq!(graph.get_oncall_string(oncall_id), Some(oncall.to_string()));

        // Store and retrieve label
        let label = "ci_test";
        let label_id = graph.store_label(label);
        assert_eq!(graph.get_label_string(label_id), Some(label.to_string()));
    }

    #[test]
    fn test_ci_deps_pattern_storage_and_retrieval() {
        let graph = TargetGraph::new();
        let target_label = "fbcode//test:target";
        let target_id = graph.store_target(target_label);

        let package_pattern = "fbcode//services/api";
        let recursive_pattern = "fbcode//core";

        let pattern_id1 = graph.store_ci_deps_pattern(package_pattern);
        let pattern_id2 = graph.store_ci_deps_pattern(recursive_pattern);

        assert_ne!(pattern_id1, pattern_id2);
        assert_eq!(
            graph.get_ci_deps_pattern_string(pattern_id1),
            Some(package_pattern.to_string())
        );
        assert_eq!(
            graph.get_ci_deps_pattern_string(pattern_id2),
            Some(recursive_pattern.to_string())
        );

        graph.store_ci_deps_package_patterns(target_id, vec![pattern_id1]);
        graph.store_ci_deps_recursive_patterns(target_id, vec![pattern_id2]);

        let retrieved_package_patterns = graph.get_ci_deps_package_patterns(target_id);
        let retrieved_recursive_patterns = graph.get_ci_deps_recursive_patterns(target_id);

        assert_eq!(retrieved_package_patterns, Some(vec![pattern_id1]));
        assert_eq!(retrieved_recursive_patterns, Some(vec![pattern_id2]));
    }

    #[test]
    fn test_minimized_target() {
        let graph = TargetGraph::new();

        let target_label = "fbcode//test:target";
        let target_id = graph.store_target(target_label);
        let rule_type_id = graph.store_rule_type("cpp_library");
        let oncall_id = graph.store_oncall("team_test");
        let label_id1 = graph.store_label("ci_test");
        let label_id2 = graph.store_label("production");

        let minimized = MinimizedBuckTarget {
            rule_type: rule_type_id,
            oncall: Some(oncall_id),
            labels: vec![label_id1, label_id2],
            target_hash: TargetHash::new("abc123def456"),
        };

        graph.store_minimized_target(target_id, minimized.clone());
        let retrieved = graph.get_minimized_target(target_id);
        assert_eq!(retrieved, Some(minimized));

        let non_existent_target_id: TargetId = "fbcode//non_existent:target".parse().unwrap();
        assert_eq!(graph.get_minimized_target(non_existent_target_id), None);
    }

    #[test]
    fn test_target_hash_in_minimized_target() {
        let graph = TargetGraph::new();

        let target_label = "fbcode//test:target";
        let target_id = graph.store_target(target_label);
        let rule_type_id = graph.store_rule_type("cpp_library");
        let target_hash = "5700a84a628259e252ef6952d6af6079";

        let minimized = MinimizedBuckTarget {
            rule_type: rule_type_id,
            oncall: None,
            labels: vec![],
            target_hash: TargetHash::new(target_hash),
        };

        graph.store_minimized_target(target_id, minimized);
        let retrieved = graph.get_minimized_target(target_id).unwrap();

        assert_eq!(retrieved.target_hash, TargetHash::new(target_hash));
    }

    #[test]
    fn test_new_extended_id_types() {
        // Test GlobPatternId
        let pattern1 = "**/*.rs";
        let pattern2 = "**/*.py";
        let pattern_id1: GlobPatternId = pattern1.parse().unwrap();
        let pattern_id2: GlobPatternId = pattern2.parse().unwrap();
        assert_ne!(pattern_id1, pattern_id2);
        assert_eq!(pattern1.parse::<GlobPatternId>().unwrap(), pattern_id1);

        // Test FileId
        let file1 = "src/main.rs";
        let file2 = "src/lib.rs";
        let file_id1: FileId = file1.parse().unwrap();
        let file_id2: FileId = file2.parse().unwrap();
        assert_ne!(file_id1, file_id2);
        assert_eq!(file1.parse::<FileId>().unwrap(), file_id1);

        // Test PackageId
        let package1 = "fbcode//target_determinator";
        let package2 = "fbcode//target_determinator/btd";
        let package_id1: PackageId = package1.parse().unwrap();
        let package_id2: PackageId = package2.parse().unwrap();
        assert_ne!(package_id1, package_id2);
        assert_eq!(package1.parse::<PackageId>().unwrap(), package_id1);

        // Test CiDepsPatternId
        let ci_pattern1 = "fbcode//services";
        let ci_pattern2 = "fbcode//tools";
        let ci_pattern_id1: CiDepsPatternId = ci_pattern1.parse().unwrap();
        let ci_pattern_id2: CiDepsPatternId = ci_pattern2.parse().unwrap();
        assert_ne!(ci_pattern_id1, ci_pattern_id2);
        assert_eq!(
            ci_pattern1.parse::<CiDepsPatternId>().unwrap(),
            ci_pattern_id1
        );
    }

    #[test]
    fn test_remove_rdep_cleans_empty_entries() {
        let graph = TargetGraph::new();

        let target1 = "fbcode//a:target1";
        let target2 = "fbcode//b:target2";
        let target3 = "fbcode//c:target3";

        let id1 = graph.store_target(target1);
        let id2 = graph.store_target(target2);
        let id3 = graph.store_target(target3);

        // Add dependencies: target1 <- target2, target1 <- target3
        graph.add_rdep(id1, id2);
        graph.add_rdep(id1, id3);

        // Verify initial state
        assert_eq!(graph.rdeps_len(), 1);
        assert_eq!(graph.deps_len(), 2);
        assert_eq!(graph.get_rdeps(id1).unwrap().len(), 2);

        // Remove one dependency
        graph.remove_rdep(id1, id2);

        // Should still have entries as id1 still has rdeps
        assert_eq!(graph.rdeps_len(), 1);
        assert_eq!(graph.deps_len(), 1);
        assert_eq!(graph.get_rdeps(id1).unwrap().len(), 1);

        // Remove the last dependency
        graph.remove_rdep(id1, id3);

        // Should have removed the empty entries
        assert_eq!(graph.rdeps_len(), 0);
        assert_eq!(graph.deps_len(), 0);
        assert!(graph.get_rdeps(id1).is_none());
    }

    #[test]
    fn test_remove_target_removes_all_data() {
        let graph = TargetGraph::new();

        let target1 = "fbcode//a:target1";
        let target2 = "fbcode//b:target2";

        let id1 = graph.store_target(target1);
        let id2 = graph.store_target(target2);

        // Add a dependency: target2 depends on target1
        // This creates: id1 -> rdeps: [id2], id2 -> deps: [id1]
        graph.add_rdep(id1, id2);

        let rule_type_id = graph.store_rule_type("cpp_library");
        let minimized = MinimizedBuckTarget {
            rule_type: rule_type_id,
            oncall: None,
            labels: vec![],
            target_hash: TargetHash::new("test_hash_123"),
        };
        graph.store_minimized_target(id1, minimized);

        // Verify initial state
        assert_eq!(graph.len(), 1); // Only id1 has minimized data
        assert_eq!(graph.rdeps_len(), 1); // id1 has rdeps
        assert_eq!(graph.deps_len(), 1); // id2 has deps
        assert!(graph.get_minimized_target(id1).is_some());

        // Remove target1
        graph.remove_target(id1);

        // len() uses minimized_targets_len, so removing minimized data decrements it
        // target_id_to_label is NOT removed (needed for removed target detection)
        assert_eq!(graph.len(), 0);
        assert!(graph.get_minimized_target(id1).is_none());

        // - id1's rdeps entry is NOT removed (still exists pointing to id2)
        // - id2's deps entry is NOT cleaned (still points to removed id1)
        // - Only id1's own deps are removed
        assert_eq!(graph.rdeps_len(), 1); // id1's rdeps entry still exists
        assert_eq!(graph.deps_len(), 1); // id2's deps entry still exists
        assert_eq!(graph.get_rdeps(id1).unwrap(), vec![id2]); // Still points to id2
        assert_eq!(graph.get_deps(id2).unwrap(), vec![id1]); // Still points to removed id1
    }

    #[test]
    fn test_package_to_targets_mapping() {
        let graph = TargetGraph::new();

        let target1 = "fbcode//foo:target1";
        let target2 = "fbcode//foo:target2";
        let target3 = "fbcode//bar:target3";

        let id1 = graph.store_target(target1);
        let id2 = graph.store_target(target2);
        let id3 = graph.store_target(target3);

        let package_foo = graph.store_package("fbcode//foo");
        let package_bar = graph.store_package("fbcode//bar");

        graph.add_target_to_package(package_foo, id1);
        graph.add_target_to_package(package_foo, id2);
        graph.add_target_to_package(package_bar, id3);

        let targets_in_foo = graph.get_targets_in_package(package_foo);
        assert_eq!(targets_in_foo, Some(vec![id1, id2]));

        let targets_in_bar = graph.get_targets_in_package(package_bar);
        assert_eq!(targets_in_bar, Some(vec![id3]));

        let empty_package = graph.store_package("fbcode//empty");
        assert_eq!(graph.get_targets_in_package(empty_package), None);
    }

    fn store_target_with_rule_type(graph: &TargetGraph, label: &str, rule_type: &str) -> TargetId {
        let target_id = graph.store_target(label);
        let rule_type_id = graph.store_rule_type(rule_type);
        graph.store_minimized_target(
            target_id,
            MinimizedBuckTarget {
                rule_type: rule_type_id,
                oncall: None,
                labels: vec![],
                target_hash: TargetHash::new("test_hash"),
            },
        );
        target_id
    }

    fn setup_ci_hint_edge(graph: &TargetGraph) -> (TargetId, TargetId) {
        let ci_hint_id =
            store_target_with_rule_type(graph, "fbcode//foo:ci_hint@my_test", CI_HINT_RULE_TYPE);
        let dest_id = store_target_with_rule_type(graph, "fbcode//foo:my_test", "python_test");
        graph.add_rdep(ci_hint_id, dest_id);
        (ci_hint_id, dest_id)
    }

    #[test]
    fn remove_ci_hint_target_cleans_reversed_edges() {
        let graph = TargetGraph::new();
        let (ci_hint_id, dest_id) = setup_ci_hint_edge(&graph);

        graph.remove_target(ci_hint_id);

        assert!(graph.get_rdeps(ci_hint_id).is_none());
        assert!(graph.get_deps(dest_id).is_none());
    }

    #[test]
    fn remove_regular_target_preserves_ci_hint_edges() {
        let graph = TargetGraph::new();
        let (ci_hint_id, dest_id) = setup_ci_hint_edge(&graph);

        graph.remove_target(dest_id);

        assert_eq!(graph.get_rdeps(ci_hint_id).unwrap(), vec![dest_id]);
        assert!(graph.get_deps(dest_id).is_none());
    }

    #[rstest]
    #[case::ci_hint_target(CI_HINT_RULE_TYPE, true)]
    #[case::regular_target("python_test", false)]
    fn is_ci_hint_based_on_rule_type(#[case] rule_type: &str, #[case] expected: bool) {
        let graph = TargetGraph::new();
        let target_id = store_target_with_rule_type(&graph, "fbcode//foo:target", rule_type);
        assert_eq!(graph.is_ci_hint_target(target_id), expected);
    }

    #[test]
    fn is_ci_hint_returns_false_for_unknown_target() {
        let graph = TargetGraph::new();
        let unknown_id = graph.store_target("fbcode//foo:unknown");
        assert!(!graph.is_ci_hint_target(unknown_id));
    }

    struct CiHintFixture {
        graph: TargetGraph,
        ci_hint: TargetId,
        target1: TargetId,
        target2: TargetId,
    }

    fn ci_hint_fixture() -> CiHintFixture {
        let graph = TargetGraph::new();
        let ci_hint = graph.store_target("fbcode//foo:ci_hint@my_test");
        let target1 = graph.store_target("fbcode//foo:target1");
        let target2 = graph.store_target("fbcode//foo:target2");
        CiHintFixture {
            graph,
            ci_hint,
            target1,
            target2,
        }
    }

    #[test]
    fn ci_hint_edge_add_creates_bidirectional_mapping() {
        let f = ci_hint_fixture();
        f.graph.add_ci_hint_edge(f.ci_hint, f.target1);
        f.graph.add_ci_hint_edge(f.ci_hint, f.target2);

        let affected = f.graph.get_ci_hint_affected(f.ci_hint).unwrap();
        assert_eq!(affected.len(), 2);
        assert!(affected.contains(&f.target1));
        assert!(affected.contains(&f.target2));

        assert_eq!(
            f.graph.get_affecting_ci_hints(f.target1).unwrap(),
            vec![f.ci_hint]
        );
        assert_eq!(
            f.graph.get_affecting_ci_hints(f.target2).unwrap(),
            vec![f.ci_hint]
        );
    }

    #[test]
    fn ci_hint_edge_does_not_pollute_rdeps_or_deps() {
        let f = ci_hint_fixture();
        f.graph.add_ci_hint_edge(f.ci_hint, f.target1);

        assert!(f.graph.get_rdeps(f.ci_hint).is_none());
        assert!(f.graph.get_rdeps(f.target1).is_none());
        assert!(f.graph.get_deps(f.ci_hint).is_none());
        assert!(f.graph.get_deps(f.target1).is_none());
    }

    #[rstest]
    #[case::partial_remove(1, true)]
    #[case::full_remove(2, false)]
    fn ci_hint_edge_remove_cleans_both_directions(
        #[case] removals: usize,
        #[case] ci_hint_has_remaining_edges: bool,
    ) {
        let f = ci_hint_fixture();
        f.graph.add_ci_hint_edge(f.ci_hint, f.target1);
        f.graph.add_ci_hint_edge(f.ci_hint, f.target2);

        f.graph.remove_ci_hint_edge(f.ci_hint, f.target1);
        if removals == 2 {
            f.graph.remove_ci_hint_edge(f.ci_hint, f.target2);
        }

        assert!(f.graph.get_affecting_ci_hints(f.target1).is_none());

        if ci_hint_has_remaining_edges {
            assert_eq!(
                f.graph.get_ci_hint_affected(f.ci_hint).unwrap(),
                vec![f.target2]
            );
            assert_eq!(
                f.graph.get_affecting_ci_hints(f.target2).unwrap(),
                vec![f.ci_hint]
            );
        } else {
            assert!(f.graph.get_ci_hint_affected(f.ci_hint).is_none());
            assert_eq!(f.graph.ci_hint_to_affected_len(), 0);
            assert_eq!(f.graph.affected_to_ci_hints_len(), 0);
        }
    }

    #[test]
    fn ci_hint_edge_multiple_ci_hints_affect_same_target() {
        let f = ci_hint_fixture();
        let ci_hint2 = f.graph.store_target("fbcode//foo:ci_hint@test2");

        f.graph.add_ci_hint_edge(f.ci_hint, f.target1);
        f.graph.add_ci_hint_edge(ci_hint2, f.target1);

        let ci_hints = f.graph.get_affecting_ci_hints(f.target1).unwrap();
        assert_eq!(ci_hints.len(), 2);
        assert!(ci_hints.contains(&f.ci_hint));
        assert!(ci_hints.contains(&ci_hint2));
    }

    #[test]
    fn ci_hint_edge_serialization_round_trip() {
        let f = ci_hint_fixture();
        f.graph.add_ci_hint_edge(f.ci_hint, f.target1);

        let json = serde_json::to_string(&f.graph).unwrap();
        let restored: TargetGraph = serde_json::from_str(&json).unwrap();

        assert_eq!(
            restored.get_ci_hint_affected(f.ci_hint).unwrap(),
            vec![f.target1]
        );
        assert_eq!(
            restored.get_affecting_ci_hints(f.target1).unwrap(),
            vec![f.ci_hint]
        );
    }

    #[test]
    fn ci_hint_edge_get_returns_none_for_unknown() {
        let graph = TargetGraph::new();
        let unknown = graph.store_target("fbcode//foo:unknown");

        assert!(graph.get_ci_hint_affected(unknown).is_none());
        assert!(graph.get_affecting_ci_hints(unknown).is_none());
    }
}
