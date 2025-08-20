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

/// A unique identifier for a target in the graph, represented as a u64 hash
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
pub struct TargetId(u64);

impl TargetId {
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl FromStr for TargetId {
    type Err = std::convert::Infallible;

    fn from_str(target: &str) -> Result<Self, Self::Err> {
        let mut hasher = FnvHasher::default();
        hasher.write(target.as_bytes());
        Ok(Self(hasher.finish()))
    }
}

/// A graph representation of targets and their reverse dependencies
#[derive(Debug, Serialize, Deserialize)]
pub struct TargetGraph {
    /// Adjacency list mapping from target IDs to their reverse dependencies (rdeps)
    /// rdeps[target] = list of targets that depend on 'target'
    rdeps: DashMap<TargetId, Vec<TargetId>>,

    /// Mapping from target label strings to their TargetId
    label_to_id: DashMap<String, TargetId>,
}

impl TargetGraph {
    pub fn new() -> Self {
        Self {
            rdeps: DashMap::new(),
            label_to_id: DashMap::new(),
        }
    }

    pub fn get_or_create_target_id(&self, target_label: &str) -> TargetId {
        if let Some(target_id) = self.label_to_id.get(target_label) {
            *target_id
        } else {
            let target_id = target_label.parse().expect("TargetId parsing never fails");
            self.label_to_id.insert(target_label.to_string(), target_id);
            target_id
        }
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
        self.label_to_id
            .iter()
            .map(|entry| *entry.value())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.label_to_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.label_to_id.is_empty()
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

        let id1 = graph.get_or_create_target_id(target1);
        let id2 = graph.get_or_create_target_id(target2);

        assert_ne!(id1, id2);

        assert_eq!(graph.len(), 2);

        // Test getting same ID for same label
        let id1_again = graph.get_or_create_target_id(target1);
        assert_eq!(id1, id1_again);
    }

    #[test]
    fn test_reverse_dependencies() {
        let graph = TargetGraph::new();

        let target1 = "fbcode//a:target1";
        let target2 = "fbcode//b:target2";
        let target3 = "fbcode//c:target3";

        let id1 = graph.get_or_create_target_id(target1);
        let id2 = graph.get_or_create_target_id(target2);
        let id3 = graph.get_or_create_target_id(target3);

        // target2 depends on target1, target3 depends on target1
        // So target1's rdeps should include target2 and target3
        graph.add_rdep(id1, id2);
        graph.add_rdep(id1, id3);

        let rdeps = graph.get_rdeps(id1).unwrap();
        assert_eq!(rdeps.len(), 2);
        assert!(rdeps.contains(&id2));
        assert!(rdeps.contains(&id3));

        // target2 and target3 should have no rdeps
        assert!(graph.get_rdeps(id2).is_none());
        assert!(graph.get_rdeps(id3).is_none());
    }

    #[test]
    fn test_serialization() {
        let graph = TargetGraph::new();

        let target1 = "fbcode//a:target1";
        let target2 = "fbcode//b:target2";
        let target3 = "fbcode//c:target3";

        let id1 = graph.get_or_create_target_id(target1);
        let id2 = graph.get_or_create_target_id(target2);
        let id3 = graph.get_or_create_target_id(target3);

        // target2 depends on target1, target3 depends on target1
        graph.add_rdep(id1, id2);
        graph.add_rdep(id1, id3);

        // Test serialization and deserialization
        let json = serde_json::to_string(&graph).expect("Failed to serialize");
        let restored_graph: TargetGraph =
            serde_json::from_str(&json).expect("Failed to deserialize");

        // Verify the restored graph has the same structure
        assert_eq!(restored_graph.len(), 3);

        let restored_rdeps1 = restored_graph.get_rdeps(id1).unwrap();
        assert_eq!(restored_rdeps1.len(), 2);
        assert!(restored_rdeps1.contains(&id2));
        assert!(restored_rdeps1.contains(&id3));

        // Verify label lookups work
        assert_eq!(restored_graph.get_or_create_target_id(target1), id1);
        assert_eq!(restored_graph.get_or_create_target_id(target2), id2);
        assert_eq!(restored_graph.get_or_create_target_id(target3), id3);
    }
}
