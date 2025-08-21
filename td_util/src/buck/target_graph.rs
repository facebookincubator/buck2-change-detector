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
use fnv::FnvHasher;
use serde::Deserialize;
use serde::Serialize;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinimizedBuckTarget {
    pub rule_type: RuleTypeId,
    pub oncall: Option<OncallId>,
    pub labels: Vec<LabelId>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TargetGraph {
    rdeps: DashMap<TargetId, Vec<TargetId>>,
    // We store BuckTargets as ids as a form of string interning
    // These maps are used to convert Ids back to strings
    target_id_to_label: DashMap<TargetId, String>,
    rule_type_id_to_string: DashMap<RuleTypeId, String>,
    oncall_id_to_string: DashMap<OncallId, String>,
    label_id_to_string: DashMap<LabelId, String>,
    minimized_targets: DashMap<TargetId, MinimizedBuckTarget>,
}

impl TargetGraph {
    pub fn new() -> Self {
        Self {
            rdeps: DashMap::new(),
            target_id_to_label: DashMap::new(),
            rule_type_id_to_string: DashMap::new(),
            oncall_id_to_string: DashMap::new(),
            label_id_to_string: DashMap::new(),
            minimized_targets: DashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.target_id_to_label.len()
    }

    pub fn is_empty(&self) -> bool {
        self.target_id_to_label.is_empty()
    }

    pub fn store_target(&self, target_label: &str) -> TargetId {
        let id = target_label.parse().unwrap();
        self.target_id_to_label.insert(id, target_label.to_string());
        id
    }

    pub fn store_rule_type(&self, rule_type: &str) -> RuleTypeId {
        let id = rule_type.parse().unwrap();
        self.rule_type_id_to_string
            .insert(id, rule_type.to_string());
        id
    }

    pub fn store_oncall(&self, oncall: &str) -> OncallId {
        let id = oncall.parse().unwrap();
        self.oncall_id_to_string.insert(id, oncall.to_string());
        id
    }

    pub fn store_label(&self, label: &str) -> LabelId {
        let id = label.parse().unwrap();
        self.label_id_to_string.insert(id, label.to_string());
        id
    }

    // Reverse lookup methods, returns string to release dashmap guard.
    pub fn get_target_label(&self, id: TargetId) -> Option<String> {
        self.target_id_to_label.get(&id).map(|v| v.clone())
    }

    pub fn get_rule_type_string(&self, id: RuleTypeId) -> Option<String> {
        self.rule_type_id_to_string.get(&id).map(|v| v.clone())
    }

    pub fn get_oncall_string(&self, id: OncallId) -> Option<String> {
        self.oncall_id_to_string.get(&id).map(|v| v.clone())
    }

    pub fn get_label_string(&self, id: LabelId) -> Option<String> {
        self.label_id_to_string.get(&id).map(|v| v.clone())
    }

    pub fn add_rdep(&self, target: TargetId, dependent_target: TargetId) {
        self.rdeps
            .entry(target)
            .or_insert_with(Vec::new)
            .push(dependent_target);
    }

    pub fn get_rdeps(&self, target_id: TargetId) -> Option<Vec<TargetId>> {
        self.rdeps.get(&target_id).map(|entry| entry.clone())
    }

    pub fn get_all_targets(&self) -> Vec<TargetId> {
        self.target_id_to_label
            .iter()
            .map(|entry| *entry.key())
            .collect()
    }

    pub fn store_minimized_target(&self, target_id: TargetId, target: MinimizedBuckTarget) {
        self.minimized_targets.insert(target_id, target);
    }

    pub fn get_minimized_target(&self, id: TargetId) -> Option<MinimizedBuckTarget> {
        self.minimized_targets.get(&id).map(|entry| entry.clone())
    }
}

impl Default for TargetGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(graph.len(), 2);

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

        assert_eq!(restored_graph.len(), 3);

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
        };

        graph.store_minimized_target(target_id, minimized.clone());
        let retrieved = graph.get_minimized_target(target_id);
        assert_eq!(retrieved, Some(minimized));

        let non_existent_target_id: TargetId = "fbcode//non_existent:target".parse().unwrap();
        assert_eq!(graph.get_minimized_target(non_existent_target_id), None);
    }
}
