/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::HashSet;
use std::hash::Hasher;
use std::io::Write;
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::PoisonError;

use dashmap::DashMap;
use dashmap::DashSet;
use rayon::prelude::*;
use rustc_hash::FxHasher;
use serde::Deserialize;
use serde::Serialize;
use td_util::no_hash::BuildNoHash;

use crate::types::Package;
use crate::types::PatternType;
use crate::types::TargetHash;
use crate::types::TargetPattern;

mod dense;

pub const CI_HINT_RULE_TYPE: &str = "ci_hint";

/// Version of the serialized graph and its persisted-state contract.
/// Version 15 stores the graph in the layout of `dense`.
/// Increment for incompatible layout or graph-maintenance changes.
pub const SCHEMA_VERSION: u32 = 15;

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
            if values.is_empty() {
                self.$map_field.remove(&key);
            } else {
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
                let mut hasher = FxHasher::default();
                hasher.write(s.as_bytes());
                Ok(Self(hasher.finish()))
            }
        }

        impl ShardKey for $name {
            fn shard_index(self, shard_count: usize) -> usize {
                (self.0 as usize) % shard_count
            }
        }

        impl RawId for $name {
            fn raw(self) -> u64 {
                self.as_u64()
            }

            fn from_raw(raw: u64) -> Self {
                Self(raw)
            }
        }
    };
}

/// Routes id-keyed entries to one of `shard_count` shards by id-modulo. Stable
/// across runs because every `Id` is a content-derived hash.
pub trait ShardKey: Copy + Eq + std::hash::Hash {
    fn shard_index(self, shard_count: usize) -> usize;
}

/// The hash inside an id, for the binary layout in `dense`.
trait RawId: ShardKey + FromStr<Err = std::convert::Infallible> + Send + Sync {
    fn raw(self) -> u64;
    fn from_raw(raw: u64) -> Self;
}

define_id_type!(TargetId);
define_id_type!(RuleTypeId);
define_id_type!(OncallId);
define_id_type!(LabelId);
define_id_type!(GlobPatternId);
define_id_type!(FileId);
define_id_type!(PackageId);
define_id_type!(CiDepsPatternId);
define_id_type!(LabelSetId);

impl LabelSetId {
    /// The id of an ordered label sequence: a hash of the label ids in order,
    /// so two targets with the same labels in the same order share one set.
    pub fn of(labels: &[LabelId]) -> Self {
        let mut hasher = FxHasher::default();
        for label in labels {
            hasher.write_u64(label.0);
        }
        Self(hasher.finish())
    }
}

pub type IdDashMap<K, V> = DashMap<K, V, BuildNoHash>;
pub type IdDashSet<K> = DashSet<K, BuildNoHash>;
pub type IdHashSet<K> = HashSet<K, BuildNoHash>;

const MAX_SHARDS: usize = 16;
const TARGET_ENTRIES_PER_SHARD: usize = 1_500_000;

/// Picks the shard count for a field given its total entry count. Aims for
/// ~1.5M entries per shard; clamps between 1 and `MAX_SHARDS`.
pub fn pick_shard_count(entry_count: usize) -> usize {
    entry_count
        .div_ceil(TARGET_ENTRIES_PER_SHARD)
        .clamp(1, MAX_SHARDS)
}

/// Generic shard container: owns `Vec<S>` of length N where each `S` is the
/// per-shard storage (e.g. `IdDashMap<K, V>` or `IdDashSet<K>`). Routing
/// dispatches by `K::shard_index`. N is fixed for the field's lifetime and
/// does not affect the binary layout. Shards are the unit of parallel
/// iteration and of parallel freeing (see `Drop for TargetGraph`).
///
/// Serializes transparently as the inner storage type (a single combined
/// `IdDashMap` or `IdDashSet`) rather than as `{"shards": [...]}`. This keeps
/// the JSON output independent of N and matches the pre-sharded format so
/// consumers (tests, debug tools) don't need to know about sharding. The
/// binary format goes through `write_framed`/`read_framed`.
#[derive(Debug)]
pub struct ShardedField<S> {
    shards: Vec<S>,
}

impl<K, V> Serialize for ShardedField<IdDashMap<K, V>>
where
    K: Eq + std::hash::Hash + Serialize,
    V: Serialize,
{
    fn serialize<Ser: serde::Serializer>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error> {
        use serde::ser::SerializeMap;
        let total = self.shards.iter().map(|s| s.len()).sum();
        let mut map = serializer.serialize_map(Some(total))?;
        for shard in &self.shards {
            for entry in shard.iter() {
                map.serialize_entry(entry.key(), entry.value())?;
            }
        }
        map.end()
    }
}

impl<'de, K, V> Deserialize<'de> for ShardedField<IdDashMap<K, V>>
where
    K: Eq + std::hash::Hash + Deserialize<'de>,
    V: Deserialize<'de>,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = IdDashMap::<K, V>::deserialize(deserializer)?;
        Ok(Self { shards: vec![map] })
    }
}

impl<K> Serialize for ShardedField<IdDashSet<K>>
where
    K: Eq + std::hash::Hash + Serialize,
{
    fn serialize<Ser: serde::Serializer>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error> {
        use serde::ser::SerializeSeq;
        let total = self.shards.iter().map(|s| s.len()).sum();
        let mut seq = serializer.serialize_seq(Some(total))?;
        for shard in &self.shards {
            for entry in shard.iter() {
                seq.serialize_element(&*entry)?;
            }
        }
        seq.end()
    }
}

impl<'de, K> Deserialize<'de> for ShardedField<IdDashSet<K>>
where
    K: Eq + std::hash::Hash + Deserialize<'de>,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let set = IdDashSet::<K>::deserialize(deserializer)?;
        Ok(Self { shards: vec![set] })
    }
}

impl<S: Default> ShardedField<S> {
    pub fn new(shard_count: usize) -> Self {
        assert!(shard_count > 0, "shard_count must be positive");
        Self {
            shards: (0..shard_count).map(|_| S::default()).collect(),
        }
    }
}

impl<S> ShardedField<S> {
    pub fn shards(&self) -> &[S] {
        &self.shards
    }

    pub fn from_shards(shards: Vec<S>) -> Self {
        assert!(!shards.is_empty(), "shards must be non-empty");
        Self { shards }
    }
}

impl<K: ShardKey, V> ShardedField<IdDashMap<K, V>> {
    pub fn shard_for(&self, key: K) -> &IdDashMap<K, V> {
        &self.shards[key.shard_index(self.shards.len())]
    }

    pub fn entry(&self, key: K) -> dashmap::mapref::entry::Entry<'_, K, V> {
        self.shard_for(key).entry(key)
    }

    pub fn insert(&self, key: K, value: V) -> Option<V> {
        self.shard_for(key).insert(key, value)
    }

    pub fn get(&self, key: &K) -> Option<dashmap::mapref::one::Ref<'_, K, V>> {
        self.shard_for(*key).get(key)
    }

    pub fn get_mut(&self, key: &K) -> Option<dashmap::mapref::one::RefMut<'_, K, V>> {
        self.shard_for(*key).get_mut(key)
    }

    pub fn remove(&self, key: &K) -> Option<(K, V)> {
        self.shard_for(*key).remove(key)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.shard_for(*key).contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| s.is_empty())
    }

    pub fn iter(&self) -> impl Iterator<Item = dashmap::mapref::multiple::RefMulti<'_, K, V>> {
        self.shards.iter().flat_map(|s| s.iter())
    }

    pub fn iter_mut(
        &self,
    ) -> impl Iterator<Item = dashmap::mapref::multiple::RefMutMulti<'_, K, V>> {
        self.shards.iter().flat_map(|s| s.iter_mut())
    }

    pub fn retain(&self, mut f: impl FnMut(&K, &mut V) -> bool) {
        for shard in &self.shards {
            shard.retain(|k, v| f(k, v));
        }
    }
}

impl<K: ShardKey + Send + Sync, V: Send + Sync> ShardedField<IdDashMap<K, V>> {
    fn par_iter(
        &self,
    ) -> impl ParallelIterator<Item = dashmap::mapref::multiple::RefMulti<'_, K, V>> {
        self.shards.par_iter().flat_map(|s| s.par_iter())
    }
}

impl<K: ShardKey> ShardedField<IdDashSet<K>> {
    pub fn shard_for(&self, key: K) -> &IdDashSet<K> {
        &self.shards[key.shard_index(self.shards.len())]
    }

    pub fn insert(&self, key: K) -> bool {
        self.shard_for(key).insert(key)
    }

    pub fn contains(&self, key: &K) -> bool {
        self.shard_for(*key).contains(key)
    }

    pub fn remove(&self, key: &K) -> Option<K> {
        self.shard_for(*key).remove(key)
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| s.is_empty())
    }

    pub fn iter(&self) -> impl Iterator<Item = dashmap::setref::multiple::RefMulti<'_, K>> {
        self.shards.iter().flat_map(|s| s.iter())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinimizedBuckTarget {
    pub rule_type: RuleTypeId,
    pub oncall: Option<OncallId>,
    pub labels: Vec<LabelId>,
    pub target_hash: TargetHash,
}

/// `MinimizedBuckTarget` as stored: the label sequence is interned in
/// `label_set_id_to_labels`, because a graph has ~10x fewer distinct label
/// sequences than targets and the per-target `Vec<LabelId>` was the single
/// largest allocation in memory and on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredMinimizedTarget {
    rule_type: RuleTypeId,
    oncall: Option<OncallId>,
    label_set: LabelSetId,
    target_hash: TargetHash,
}

/// Single source of truth for `TargetGraph`'s field set. Generates the struct,
/// whose fields are each sharded in memory by id, and the shard count and
/// hand-off that `Drop` uses. `dense` owns the binary layout that
/// `write_framed` writes and `read_framed` reads.
macro_rules! define_target_graph {
    ($($field:ident: $ty:ty),* $(,)?) => {
        #[derive(Debug, Serialize, Deserialize)]
        pub struct TargetGraph {
            $($field: ShardedField<$ty>,)*
        }

        const NUM_FIELDS: usize = [$(stringify!($field)),*].len();

        impl TargetGraph {
            /// The number of shards over all fields.
            fn shard_count(&self) -> usize {
                0 $(+ self.$field.shards.len())*
            }

            /// Moves the shards of every field out of the graph, each with its
            /// entry count, so they can be freed in parallel. Leaves every
            /// field without shards; see `free_shards_on_threads`.
            fn take_all_shards(&mut self) -> Vec<(usize, Box<dyn Send>)> {
                let mut shards: Vec<(usize, Box<dyn Send>)> = Vec::new();
                $(
                    shards.extend(
                        std::mem::take(&mut self.$field.shards)
                            .into_iter()
                            .map(|shard| (shard.len(), Box::new(shard) as Box<dyn Send>)),
                    );
                )*
                shards
            }
        }
    };
}

define_target_graph! {
    target_id_to_label: IdDashMap<TargetId, String>,
    rule_type_id_to_string: IdDashMap<RuleTypeId, String>,
    oncall_id_to_string: IdDashMap<OncallId, String>,
    label_id_to_string: IdDashMap<LabelId, String>,
    minimized_targets: IdDashMap<TargetId, StoredMinimizedTarget>,
    label_set_id_to_labels: IdDashMap<LabelSetId, Vec<LabelId>>,
    glob_pattern_id_to_string: IdDashMap<GlobPatternId, String>,
    package_id_to_path: IdDashMap<PackageId, String>,
    file_id_to_path: IdDashMap<FileId, String>,
    ci_deps_pattern_id_to_string: IdDashMap<CiDepsPatternId, String>,
    target_id_to_rdeps: IdDashMap<TargetId, Vec<TargetId>>,
    target_id_to_deps: IdDashMap<TargetId, Vec<TargetId>>,
    file_id_to_rdeps: IdDashMap<FileId, Vec<FileId>>,
    file_id_to_deps: IdDashMap<FileId, Vec<FileId>>,
    package_id_to_errors: IdDashMap<PackageId, Vec<String>>,
    package_id_to_targets: IdDashMap<PackageId, Vec<TargetId>>,
    target_id_to_ci_srcs: IdDashMap<TargetId, Vec<GlobPatternId>>,
    target_id_to_ci_srcs_must_match: IdDashMap<TargetId, Vec<GlobPatternId>>,
    target_id_to_ci_deps_package_patterns: IdDashMap<TargetId, Vec<CiDepsPatternId>>,
    target_id_to_ci_deps_recursive_patterns: IdDashMap<TargetId, Vec<CiDepsPatternId>>,
    targets_with_sudo_label: IdDashSet<TargetId>,
    ci_hint_to_affected: IdDashMap<TargetId, Vec<TargetId>>,
    affected_to_ci_hints: IdDashMap<TargetId, Vec<TargetId>>,
}

impl TargetGraph {
    /// Writes the graph in the binary layout of graph schema `SCHEMA_VERSION`.
    pub fn write_framed(&self, writer: &mut dyn Write) -> anyhow::Result<()> {
        dense::write(self, writer)
    }

    pub fn read_framed(file: &std::fs::File) -> anyhow::Result<Self> {
        Self::read_framed_cancellable(file, &|| Ok(()))
    }

    /// Reads a graph written by `write_framed`, checking cancellation before
    /// decoding each block.
    pub fn read_framed_cancellable(
        file: &std::fs::File,
        check_cancelled: &(dyn Fn() -> anyhow::Result<()> + Sync),
    ) -> anyhow::Result<Self> {
        dense::read(file, check_cancelled)
    }
}

impl TargetGraph {
    pub fn new() -> Self {
        // Every field starts with a single shard; loading a graph picks a
        // shard count from each field's size.
        Self {
            target_id_to_label: ShardedField::new(1),
            rule_type_id_to_string: ShardedField::new(1),
            oncall_id_to_string: ShardedField::new(1),
            label_id_to_string: ShardedField::new(1),
            minimized_targets: ShardedField::new(1),
            label_set_id_to_labels: ShardedField::new(1),
            glob_pattern_id_to_string: ShardedField::new(1),
            target_id_to_rdeps: ShardedField::new(1),
            target_id_to_deps: ShardedField::new(1),
            file_id_to_path: ShardedField::new(1),
            file_id_to_rdeps: ShardedField::new(1),
            file_id_to_deps: ShardedField::new(1),
            package_id_to_path: ShardedField::new(1),
            package_id_to_errors: ShardedField::new(1),
            package_id_to_targets: ShardedField::new(1),
            ci_deps_pattern_id_to_string: ShardedField::new(1),
            target_id_to_ci_srcs: ShardedField::new(1),
            target_id_to_ci_srcs_must_match: ShardedField::new(1),
            target_id_to_ci_deps_package_patterns: ShardedField::new(1),
            target_id_to_ci_deps_recursive_patterns: ShardedField::new(1),
            targets_with_sudo_label: ShardedField::new(1),
            ci_hint_to_affected: ShardedField::new(1),
            affected_to_ci_hints: ShardedField::new(1),
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

    pub fn clear_errors(&self, package_id: PackageId) {
        self.package_id_to_errors.remove(&package_id);
    }

    pub fn error_package_ids(&self) -> IdHashSet<PackageId> {
        self.package_id_to_errors
            .iter()
            .map(|entry| *entry.key())
            .collect()
    }

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

    /// Creates the same edges as `add_rdep(dep, target_id)` for every dep, but
    /// takes the forward-edge vector as is, so a vector built at the right size
    /// stays at the right size instead of being regrown one `push` at a time.
    pub fn store_deps_with_rdeps(&self, target_id: TargetId, deps: Vec<TargetId>) {
        for &dep_id in &deps {
            self.target_id_to_rdeps
                .entry(dep_id)
                .or_default()
                .push(target_id);
        }
        if deps.is_empty() {
            return;
        }
        match self.target_id_to_deps.entry(target_id) {
            dashmap::mapref::entry::Entry::Occupied(mut existing) => {
                existing.get_mut().extend(deps);
            }
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(deps);
            }
        }
    }

    /// Release the capacity slack of every edge vector. During construction
    /// every edge list is grown by `push`, so most carry slack.
    pub fn shrink_edge_vectors(&self) {
        fn shrink_all<K: ShardKey + Send + Sync, V: Send + Sync>(
            field: &ShardedField<IdDashMap<K, Vec<V>>>,
        ) {
            field.shards().par_iter().for_each(|shard| {
                shard
                    .par_iter_mut()
                    .for_each(|mut entry| entry.value_mut().shrink_to_fit());
            });
        }
        shrink_all(&self.target_id_to_rdeps);
        shrink_all(&self.target_id_to_deps);
        shrink_all(&self.package_id_to_targets);
        shrink_all(&self.file_id_to_rdeps);
        shrink_all(&self.file_id_to_deps);
        shrink_all(&self.ci_hint_to_affected);
        shrink_all(&self.affected_to_ci_hints);
    }

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

    pub fn get_rdeps(&self, target_id: TargetId) -> Option<Vec<TargetId>> {
        self.target_id_to_rdeps.get(&target_id).map(|v| v.clone())
    }

    /// Appends the target's rdeps and ci_hint-affected targets to `out` without
    /// cloning the stored vectors. `out` is not cleared, so a caller can reuse
    /// one buffer across a traversal. Same contents as `get_all_dependents`.
    pub fn dependents_into(&self, target_id: TargetId, out: &mut Vec<TargetId>) {
        if let Some(rdeps) = self.target_id_to_rdeps.get(&target_id) {
            out.extend_from_slice(&rdeps);
        }
        if let Some(affected) = self.ci_hint_to_affected.get(&target_id) {
            out.extend_from_slice(&affected);
        }
    }

    /// Appends the target's deps to `out` without cloning the stored vector.
    pub fn deps_into(&self, target_id: TargetId, out: &mut Vec<TargetId>) {
        if let Some(deps) = self.target_id_to_deps.get(&target_id) {
            out.extend_from_slice(&deps);
        }
    }

    /// Whether the target has any (regular) dependency edge.
    pub fn has_deps(&self, target_id: TargetId) -> bool {
        self.target_id_to_deps
            .get(&target_id)
            .is_some_and(|deps| !deps.is_empty())
    }

    /// Whether anything depends on the target via an rdep or ci_hint edge.
    pub fn has_dependents(&self, target_id: TargetId) -> bool {
        self.target_id_to_rdeps
            .get(&target_id)
            .is_some_and(|rdeps| !rdeps.is_empty())
            || self
                .ci_hint_to_affected
                .get(&target_id)
                .is_some_and(|affected| !affected.is_empty())
    }

    /// The target's rule type id, read without cloning its `MinimizedBuckTarget`.
    pub fn rule_type_id(&self, target_id: TargetId) -> Option<RuleTypeId> {
        self.minimized_targets
            .get(&target_id)
            .map(|minimized| minimized.rule_type)
    }

    /// Whether the target carries `label_id`, read without cloning its labels.
    pub fn target_has_label(&self, target_id: TargetId, label_id: LabelId) -> bool {
        let Some(label_set) = self
            .minimized_targets
            .get(&target_id)
            .map(|minimized| minimized.label_set)
        else {
            return false;
        };
        self.with_label_set(label_set, |labels| labels.contains(&label_id))
    }

    pub fn contains_target(&self, target_id: TargetId) -> bool {
        self.target_id_to_label.contains_key(&target_id)
    }

    /// Runs `f` on the target's label without cloning it. `f` runs under the
    /// read lock of one `target_id_to_label` shard, so it must not call a
    /// method that writes to that map (`store_target`, `remove_target`, ...),
    /// which would deadlock on the same shard. Nested reads are fine.
    pub fn with_target_label<R>(
        &self,
        target_id: TargetId,
        f: impl FnOnce(&str) -> R,
    ) -> Option<R> {
        self.target_id_to_label
            .get(&target_id)
            .map(|label| f(label.as_str()))
    }

    /// Whether the rule type's short name (after the last `:`) is `ci_hint`.
    pub fn is_ci_hint_rule_type(&self, rule_type_id: RuleTypeId) -> bool {
        self.rule_type_id_to_string
            .get(&rule_type_id)
            .is_some_and(|full| {
                full.rsplit_once(':')
                    .map_or(full.as_str(), |(_, short)| short)
                    == CI_HINT_RULE_TYPE
            })
    }

    pub fn get_deps(&self, target_id: TargetId) -> Option<Vec<TargetId>> {
        self.target_id_to_deps.get(&target_id).map(|v| v.clone())
    }

    pub fn set_deps(&self, target_id: TargetId, deps: Vec<TargetId>) {
        if deps.is_empty() {
            self.target_id_to_deps.remove(&target_id);
        } else {
            self.target_id_to_deps.insert(target_id, deps);
        }
    }

    pub fn add_to_rdeps(&self, dep_id: TargetId, dependent_target: TargetId) {
        self.target_id_to_rdeps
            .entry(dep_id)
            .or_default()
            .push(dependent_target);
    }

    pub fn batch_update_rdeps(
        &self,
        additions: IdDashMap<TargetId, Vec<TargetId>>,
        removals: IdDashMap<TargetId, Vec<TargetId>>,
    ) {
        removals.into_par_iter().for_each(|(dep_id, to_remove)| {
            let remove_set: IdHashSet<TargetId> = to_remove.into_iter().collect();
            let Some(mut rdeps) = self.target_id_to_rdeps.get_mut(&dep_id) else {
                return;
            };
            rdeps.retain(|id| !remove_set.contains(id));
            if rdeps.is_empty() {
                drop(rdeps);
                self.target_id_to_rdeps.remove(&dep_id);
            }
        });

        additions.into_par_iter().for_each(|(dep_id, to_add)| {
            self.target_id_to_rdeps
                .entry(dep_id)
                .or_default()
                .extend(to_add);
        });
    }

    pub fn remove_from_rdeps(&self, dep_id: TargetId, target_to_remove: TargetId) {
        if let Some(mut rdeps) = self.target_id_to_rdeps.get_mut(&dep_id) {
            if let Some(pos) = rdeps.iter().position(|&id| id == target_to_remove) {
                rdeps.swap_remove(pos);
            }
            if rdeps.is_empty() {
                drop(rdeps);
                self.target_id_to_rdeps.remove(&dep_id);
            }
        }
    }

    pub fn remove_target(&self, target_id: TargetId) {
        if self.is_ci_hint_target(target_id) {
            self.remove_ci_hint_target(target_id);
        } else {
            self.remove_regular_target(target_id);
        }

        self.remove_target_from_package(target_id);
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

    /// The package a target is filed under, derived from its label: a label
    /// is `<package>:<name>`, and a package id is the hash of its path.
    fn package_of(&self, target_id: TargetId) -> Option<PackageId> {
        self.with_target_label(target_id, |label| {
            label
                .rsplit_once(':')
                .map(|(package, _)| package.parse().expect("id parsing is infallible"))
        })
        .flatten()
    }

    /// Removes `target_id` from its derived package. Returns false if it is
    /// not filed there, so the caller can fall back to scanning.
    fn remove_target_from_derived_package(&self, target_id: TargetId) -> bool {
        let Some(package_id) = self.package_of(target_id) else {
            return false;
        };
        let Some(mut targets) = self.package_id_to_targets.get_mut(&package_id) else {
            return false;
        };
        let Some(pos) = targets.iter().position(|&id| id == target_id) else {
            return false;
        };
        targets.swap_remove(pos);
        if targets.is_empty() {
            drop(targets);
            self.package_id_to_targets.remove(&package_id);
        }
        true
    }

    fn remove_target_from_package(&self, target_id: TargetId) {
        if self.remove_target_from_derived_package(target_id) {
            return;
        }
        for mut entry in self.package_id_to_targets.iter_mut() {
            let targets = entry.value_mut();
            let Some(pos) = targets.iter().position(|&id| id == target_id) else {
                continue;
            };
            targets.swap_remove(pos);
            if targets.is_empty() {
                let package_id = *entry.key();
                drop(entry);
                self.package_id_to_targets.remove(&package_id);
            }
            return;
        }
    }

    fn remove_ci_hint_target(&self, target_id: TargetId) {
        // Clean up CI hint edges from dedicated maps
        if let Some(affected) = self.get_ci_hint_affected(target_id) {
            for affected_target in affected {
                if let Some(mut ci_hints) = self.affected_to_ci_hints.get_mut(&affected_target) {
                    if let Some(pos) = ci_hints.iter().position(|&id| id == target_id) {
                        ci_hints.swap_remove(pos);
                    }
                    if ci_hints.is_empty() {
                        drop(ci_hints);
                        self.affected_to_ci_hints.remove(&affected_target);
                    }
                }
            }
        }
        self.ci_hint_to_affected.remove(&target_id);

        // Clean up any regular dep edges (from ci_deps concrete labels)
        if let Some(deps) = self.get_deps(target_id) {
            for dep_id in deps {
                self.remove_from_rdeps(dep_id, target_id);
            }
        }
    }

    fn remove_regular_target(&self, target_id: TargetId) {
        // Clean downward: remove ourselves from our deps' rdep lists
        if let Some(deps) = self.get_deps(target_id) {
            for dep_id in deps {
                self.remove_from_rdeps(dep_id, target_id);
            }
        }

        // Leave CI hint edges intact: if a CI hint H affects this target,
        // ci_hint_to_affected[H] still contains target_id (dangling).
        // This matches the pattern for regular rdeps — upward references
        // to removed targets are preserved for detection.
    }

    pub fn remove_targets_batch(&self, targets_to_remove: &IdHashSet<TargetId>) {
        if targets_to_remove.is_empty() {
            return;
        }

        self.clean_ci_hint_edges_for_removed_targets(targets_to_remove);
        self.clean_rdeps_for_removed_targets(targets_to_remove);
        self.clean_package_targets_for_removed_targets(targets_to_remove);
        self.clean_per_target_data(targets_to_remove);
    }

    fn clean_ci_hint_edges_for_removed_targets(&self, targets_to_remove: &IdHashSet<TargetId>) {
        for &target_id in targets_to_remove {
            if !self.is_ci_hint_target(target_id) {
                continue;
            }

            for affected_target in self.get_ci_hint_affected(target_id).unwrap_or_default() {
                let Some(mut ci_hints) = self.affected_to_ci_hints.get_mut(&affected_target) else {
                    continue;
                };
                ci_hints.retain(|id| !targets_to_remove.contains(id));
                if ci_hints.is_empty() {
                    drop(ci_hints);
                    self.affected_to_ci_hints.remove(&affected_target);
                }
            }

            self.ci_hint_to_affected.remove(&target_id);
        }
    }

    fn clean_rdeps_for_removed_targets(&self, targets_to_remove: &IdHashSet<TargetId>) {
        let removals: IdDashMap<TargetId, Vec<TargetId>> = IdDashMap::default();
        for &tid in targets_to_remove {
            for dep_id in self.get_deps(tid).unwrap_or_default() {
                removals.entry(dep_id).or_default().push(tid);
            }
        }
        self.batch_update_rdeps(IdDashMap::default(), removals);
    }

    /// Removes the targets from their packages. Each target is looked up in
    /// the package its label names, so the cost scales with the removal
    /// rather than with the graph; only targets not filed under that package
    /// fall back to scanning every package.
    fn clean_package_targets_for_removed_targets(&self, targets_to_remove: &IdHashSet<TargetId>) {
        let unfiled: IdHashSet<TargetId> = targets_to_remove
            .iter()
            .copied()
            .filter(|&target_id| !self.remove_target_from_derived_package(target_id))
            .collect();
        if unfiled.is_empty() {
            return;
        }
        self.package_id_to_targets.retain(|_, targets| {
            targets.retain(|id| !unfiled.contains(id));
            !targets.is_empty()
        });
    }

    fn clean_per_target_data(&self, targets_to_remove: &IdHashSet<TargetId>) {
        for &target_id in targets_to_remove {
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
    }

    pub fn get_all_targets(&self) -> impl Iterator<Item = TargetId> + '_ {
        self.target_id_to_label.iter().map(|entry| *entry.key())
    }

    pub fn store_minimized_target(&self, target_id: TargetId, target: MinimizedBuckTarget) {
        let MinimizedBuckTarget {
            rule_type,
            oncall,
            labels,
            target_hash,
        } = target;
        let label_set = self.store_label_set(labels);
        self.minimized_targets.insert(
            target_id,
            StoredMinimizedTarget {
                rule_type,
                oncall,
                label_set,
                target_hash,
            },
        );
    }

    pub fn get_minimized_target(&self, id: TargetId) -> Option<MinimizedBuckTarget> {
        let stored = self.minimized_targets.get(&id)?.clone();
        Some(MinimizedBuckTarget {
            rule_type: stored.rule_type,
            oncall: stored.oncall,
            labels: self.get_label_set(stored.label_set),
            target_hash: stored.target_hash,
        })
    }

    /// Intern an ordered label sequence and return its id. Most calls hit an
    /// already-interned sequence, so the shard is probed under a read lock
    /// first.
    pub fn store_label_set(&self, labels: Vec<LabelId>) -> LabelSetId {
        let id = LabelSetId::of(&labels);
        if let Some(interned) = self.label_set_id_to_labels.get(&id) {
            debug_assert_eq!(
                *interned, labels,
                "two different label sequences hashed to {id:?}"
            );
            return id;
        }
        match self.label_set_id_to_labels.entry(id) {
            dashmap::mapref::entry::Entry::Occupied(interned) => {
                debug_assert_eq!(
                    *interned.get(),
                    labels,
                    "two different label sequences hashed to {id:?}"
                );
            }
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(labels);
            }
        }
        id
    }

    /// Runs `f` on an interned label sequence without cloning it. `f` runs
    /// under the read lock of one `label_set_id_to_labels` shard, so it must
    /// not intern another label set.
    ///
    /// Panics on an id that was never interned: every id in the graph comes
    /// from `store_label_set`, so an unknown one means the graph was built or
    /// loaded wrong, and treating it as "no labels" would silently drop the
    /// target's CI labels.
    fn with_label_set<R>(&self, id: LabelSetId, f: impl FnOnce(&[LabelId]) -> R) -> R {
        let interned = self
            .label_set_id_to_labels
            .get(&id)
            .unwrap_or_else(|| panic!("label set {id:?} is not interned in this graph"));
        f(interned.value())
    }

    /// The labels of an interned sequence. Panics on an unknown id, see
    /// `with_label_set`.
    pub fn get_label_set(&self, id: LabelSetId) -> Vec<LabelId> {
        self.with_label_set(id, |labels| labels.to_vec())
    }

    pub fn label_sets_len(&self) -> usize {
        self.label_set_id_to_labels.len()
    }

    pub fn is_ci_hint_target(&self, target_id: TargetId) -> bool {
        self.rule_type_id(target_id)
            .is_some_and(|rule_type_id| self.is_ci_hint_rule_type(rule_type_id))
    }

    pub fn get_rule_type_short(&self, rule_type_id: RuleTypeId) -> Option<String> {
        self.get_rule_type_string(rule_type_id).map(|full| {
            full.rsplit_once(':')
                .map_or(full.as_str(), |(_, short)| short)
                .to_string()
        })
    }

    pub fn get_rule_type_file(&self, rule_type_id: RuleTypeId) -> Option<String> {
        self.get_rule_type_string(rule_type_id)
            .and_then(|full| full.rsplit_once(':').map(|(file, _)| file.to_string()))
    }

    pub fn mark_target_has_sudo_label(&self, target_id: TargetId) {
        self.targets_with_sudo_label.insert(target_id);
    }

    pub fn unmark_target_has_sudo_label(&self, target_id: TargetId) {
        self.targets_with_sudo_label.remove(&target_id);
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

    /// Every target carrying any of `label_ids`. Resolves the matching label
    /// sets eagerly, before the returned iterator is advanced, then streams
    /// the targets.
    pub fn iter_targets_with_any_label(
        &self,
        label_ids: &IdHashSet<LabelId>,
    ) -> impl Iterator<Item = TargetId> + '_ {
        let matching_sets: IdHashSet<LabelSetId> = self
            .label_set_id_to_labels
            .iter()
            .filter(|entry| entry.value().iter().any(|lid| label_ids.contains(lid)))
            .map(|entry| *entry.key())
            .collect();
        self.minimized_targets.iter().filter_map(move |entry| {
            let target_id = *entry.key();
            if matching_sets.contains(&entry.value().label_set) {
                Some(target_id)
            } else {
                None
            }
        })
    }

    // Size analysis methods
    pub fn rdeps_len(&self) -> usize {
        self.target_id_to_rdeps.len()
    }

    pub fn deps_len(&self) -> usize {
        self.target_id_to_deps.len()
    }

    // Bidirectional file import tracking for BZL imports
    pub fn add_file_import(&self, imported: FileId, importer: FileId) {
        self.file_id_to_rdeps
            .entry(imported)
            .or_default()
            .push(importer);

        self.file_id_to_deps
            .entry(importer)
            .or_default()
            .push(imported);
    }

    pub fn get_file_rdeps(&self, file_id: FileId) -> Option<Vec<FileId>> {
        self.file_id_to_rdeps.get(&file_id).map(|v| v.clone())
    }

    pub fn contains_file(&self, file_id: FileId) -> bool {
        self.file_id_to_path.contains_key(&file_id)
    }

    /// Runs `f` on the file path without cloning or interning it. `f` runs
    /// under the read lock of one `file_id_to_path` shard, so it must not call
    /// a method that writes to that map (`store_file`, `remove_file`, ...),
    /// which would deadlock on the same shard. Nested reads are fine.
    pub fn with_file_path<R>(&self, file_id: FileId, f: impl FnOnce(&str) -> R) -> Option<R> {
        self.file_id_to_path
            .get(&file_id)
            .map(|path| f(path.as_str()))
    }

    pub fn get_file_deps(&self, file_id: FileId) -> Option<Vec<FileId>> {
        self.file_id_to_deps.get(&file_id).map(|v| v.clone())
    }

    pub fn set_file_deps(&self, importer: FileId, deps: Vec<FileId>) {
        if deps.is_empty() {
            self.file_id_to_deps.remove(&importer);
        } else {
            self.file_id_to_deps.insert(importer, deps);
        }
    }

    pub fn add_to_file_rdeps(&self, imported: FileId, importer: FileId) {
        self.file_id_to_rdeps
            .entry(imported)
            .or_default()
            .push(importer);
    }

    pub fn remove_from_file_rdeps(&self, imported: FileId, importer: FileId) {
        if let Some(mut rdeps) = self.file_id_to_rdeps.get_mut(&imported) {
            if let Some(pos) = rdeps.iter().position(|&id| id == importer) {
                rdeps.swap_remove(pos);
            }
            if rdeps.is_empty() {
                drop(rdeps);
                self.file_id_to_rdeps.remove(&imported);
            }
        }
    }

    pub fn clear_file_deps(&self, importer: FileId) {
        if let Some((_, deps)) = self.file_id_to_deps.remove(&importer) {
            for dep in deps {
                if let Some(mut rdeps) = self.file_id_to_rdeps.get_mut(&dep) {
                    if let Some(pos) = rdeps.iter().position(|&id| id == importer) {
                        rdeps.swap_remove(pos);
                    }
                    if rdeps.is_empty() {
                        drop(rdeps);
                        self.file_id_to_rdeps.remove(&dep);
                    }
                }
            }
        }
    }

    pub fn file_rdeps_len(&self) -> usize {
        self.file_id_to_rdeps.len()
    }

    pub fn file_deps_len(&self) -> usize {
        self.file_id_to_deps.len()
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
            ("label_sets", self.label_sets_len()),
            ("minimized_targets", self.minimized_targets_len()),
            ("glob_patterns", self.glob_patterns_len()),
            ("files", self.files_len()),
            ("file_rdeps", self.file_rdeps_len()),
            ("file_deps", self.file_deps_len()),
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

    pub fn iter_packages_with_targets(&self) -> impl Iterator<Item = (PackageId, String)> + '_ {
        self.package_id_to_targets.iter().filter_map(|entry| {
            let package_id = *entry.key();
            self.get_package_path(package_id)
                .map(|path| (package_id, path))
        })
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
            if let Some(pos) = affected.iter().position(|&id| id == affected_target) {
                affected.swap_remove(pos);
            }
            if affected.is_empty() {
                drop(affected);
                self.ci_hint_to_affected.remove(&ci_hint_id);
            }
        }
        if let Some(mut ci_hints) = self.affected_to_ci_hints.get_mut(&affected_target) {
            if let Some(pos) = ci_hints.iter().position(|&id| id == ci_hint_id) {
                ci_hints.swap_remove(pos);
            }
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

    pub fn get_all_dependents(&self, target_id: TargetId) -> Option<Vec<TargetId>> {
        let mut result = self.get_rdeps(target_id).unwrap_or_default();
        result.extend(self.get_ci_hint_affected(target_id).unwrap_or_default());

        if result.is_empty() {
            None
        } else {
            Some(result)
        }
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

/// Graphs passed to [`TargetGraph::keep_until_exit`]. Rust never drops a
/// static, and the static keeps each graph reachable, so LeakSanitizer does
/// not report it.
static KEPT_UNTIL_EXIT: Mutex<Vec<TargetGraph>> = Mutex::new(Vec::new());

impl TargetGraph {
    /// Keeps the graph alive until the process exits, and never frees it.
    ///
    /// Freeing a production graph walks tens of millions of allocations,
    /// which takes time even in parallel. The graph owns nothing but memory,
    /// and the kernel reclaims the whole address space at exit much faster.
    /// Call this only from a command's entry point, right before the process
    /// exits.
    pub fn keep_until_exit(self) {
        KEPT_UNTIL_EXIT
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(self);
    }
}

/// Keeps small graphs with several shards per field, such as test graphs, off
/// the threads. Estimated from the free time of a production graph of 27M
/// targets, a graph below this size frees in about 5 ms on one thread.
const PARALLEL_DROP_TARGETS: usize = 100_000;
/// Threads that free a large graph, counting the dropping thread. On a
/// production graph of 27M targets read from a file, more threads stopped
/// helping at about 64.
const DROP_THREADS: usize = 64;

/// Frees the shards of every field on short-lived threads, because a large
/// graph holds tens of millions of small allocations. Each shard is one unit
/// of work. Dedicated threads, not the rayon pool, keep an implicit drop
/// independent of the pool's work and of the locks that the dropping thread
/// holds.
impl Drop for TargetGraph {
    fn drop(&mut self) {
        self.free_shards_on_threads(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    }
}

impl TargetGraph {
    /// Frees the shards on threads if `free_threads` chooses more than one.
    /// Leaves every field without shards in that case, so only `Drop` and
    /// tests call it.
    fn free_shards_on_threads(&mut self, parallelism: impl FnOnce() -> usize) {
        let threads = free_threads(self.targets_len(), self.shard_count(), parallelism);
        if threads <= 1 {
            return;
        }
        let mut shards = self.take_all_shards();
        // Threads take shards from the end, so the largest shards start first.
        shards.sort_unstable_by_key(|(entries, _)| *entries);
        free_in_parallel(shards, threads);
    }
}

/// Threads that free a graph of `targets` targets in `shards` shards,
/// counting the dropping thread. One means that the dropping thread frees the
/// graph alone, as for any value.
///
/// Only a field with more than 1.5M entries is read from a file as several
/// shards, usually by several decoding threads, so more shards than fields
/// marks a large graph read from a file. A graph with one shard per field,
/// loaded from JSON, freed more slowly on threads than on the dropping thread.
fn free_threads(targets: usize, shards: usize, parallelism: impl FnOnce() -> usize) -> usize {
    if targets < PARALLEL_DROP_TARGETS || shards <= NUM_FIELDS {
        return 1;
    }
    parallelism().min(DROP_THREADS).min(shards)
}

/// Frees `items` on `threads` threads, counting the calling thread. Threads
/// take items from the end of `items`.
fn free_in_parallel<T: Send>(items: Vec<T>, threads: usize) {
    let items = Mutex::new(items);
    // The lock guard ends inside `next`, so items free concurrently.
    let next = || items.lock().unwrap_or_else(PoisonError::into_inner).pop();
    let free = || {
        while let Some(item) = next() {
            drop(item);
        }
    };
    std::thread::scope(|scope| {
        for _ in 1..threads {
            // If a thread fails to start, the calling thread frees its share.
            if let Err(err) = std::thread::Builder::new()
                .name("tgraph-drop".to_owned())
                .spawn_scoped(scope, free)
            {
                tracing::warn!("freeing the target graph on fewer threads: {err}");
                break;
            }
        }
        free();
    });
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[test]
    fn kept_graphs_stay_reachable() {
        let graph = TargetGraph::new();
        let target = graph.store_target("//pkg:kept");

        graph.keep_until_exit();

        // LeakSanitizer reports only memory that no static or stack can reach.
        let kept = KEPT_UNTIL_EXIT
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            kept.iter()
                .any(|graph| graph.get_target_label(target).as_deref() == Some("//pkg:kept"))
        );
    }

    #[rstest]
    #[case::small_graph(PARALLEL_DROP_TARGETS - 1, 4 * NUM_FIELDS, 192, 1)]
    #[case::one_shard_per_field(PARALLEL_DROP_TARGETS, NUM_FIELDS, 192, 1)]
    #[case::capped_by_constant(PARALLEL_DROP_TARGETS, 4 * NUM_FIELDS, 192, DROP_THREADS)]
    #[case::capped_by_parallelism(PARALLEL_DROP_TARGETS, 4 * NUM_FIELDS, 8, 8)]
    #[case::capped_by_shards(PARALLEL_DROP_TARGETS, NUM_FIELDS + 1, 192, NUM_FIELDS + 1)]
    fn free_threads_parallelizes_only_large_multi_shard_graphs(
        #[case] targets: usize,
        #[case] shards: usize,
        #[case] parallelism: usize,
        #[case] expected: usize,
    ) {
        assert_eq!(free_threads(targets, shards, || parallelism), expected);
    }

    /// A large graph with a multi-shard field takes the threaded path, which
    /// frees every shard.
    #[test]
    fn a_large_multi_shard_graph_frees_on_threads() {
        let mut graph = TargetGraph::new();
        graph.target_id_to_label = ShardedField::new(4);
        for i in 0..PARALLEL_DROP_TARGETS {
            graph.store_target(&format!("//pkg:t{i}"));
        }
        graph.free_shards_on_threads(|| 8);
        assert_eq!(graph.shard_count(), 0);
    }

    /// Each item waits in its drop until `threads` items are being dropped at
    /// the same time. There are twice as many items as threads, so each thread
    /// must also keep taking items. A wait that times out fails the test
    /// instead of hanging it.
    #[test]
    fn free_in_parallel_frees_items_concurrently() {
        use std::sync::Arc;
        use std::sync::Condvar;
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        struct Gate {
            arrived: Mutex<usize>,
            changed: Condvar,
            group: usize,
            timed_out: AtomicBool,
        }
        struct Rendezvous(Arc<Gate>);
        impl Drop for Rendezvous {
            fn drop(&mut self) {
                let gate = &self.0;
                if gate.timed_out.load(Ordering::Relaxed) {
                    return;
                }
                let mut arrived = gate.arrived.lock().unwrap();
                *arrived += 1;
                let group_end = arrived.div_ceil(gate.group) * gate.group;
                gate.changed.notify_all();
                let (arrived, wait) = gate
                    .changed
                    .wait_timeout_while(arrived, Duration::from_secs(30), |a| *a < group_end)
                    .unwrap();
                drop(arrived);
                if wait.timed_out() {
                    gate.timed_out.store(true, Ordering::Relaxed);
                }
            }
        }

        let threads = 8;
        let gate = Arc::new(Gate {
            arrived: Mutex::new(0),
            changed: Condvar::new(),
            group: threads,
            timed_out: AtomicBool::new(false),
        });
        let items = (0..2 * threads).map(|_| Rendezvous(gate.clone())).collect();
        free_in_parallel(items, threads);
        assert!(!gate.timed_out.load(Ordering::Relaxed));
        assert_eq!(*gate.arrived.lock().unwrap(), 2 * threads);
    }

    fn build_distinguishing_graph() -> TargetGraph {
        let graph = TargetGraph::new();
        graph.store_target("//pkg:t0");
        for i in 0..2 {
            graph.store_rule_type(&format!("rule_{i}"));
        }
        for i in 0..3 {
            graph.store_oncall(&format!("oncall_{i}"));
        }
        for i in 0..4 {
            graph.store_label(&format!("label_{i}"));
        }
        for i in 0..5 {
            graph.store_glob_pattern(&format!("glob/{i}/**"));
        }
        for i in 0..6 {
            graph.store_package(&format!("pkg/{i}"));
        }
        for i in 0..7 {
            graph.store_file(&format!("file/{i}.bzl"));
        }
        for i in 0..8 {
            graph.store_ci_deps_pattern(&format!("dep_pat/{i}"));
        }
        graph
    }

    /// Write framed bytes to an anonymous tempfile and read them back;
    /// `read_framed` uses positioned reads, so tests must go through a real file.
    fn read_framed_bytes(bytes: &[u8]) -> anyhow::Result<TargetGraph> {
        let mut tmp = tempfile::tempfile().unwrap();
        tmp.write_all(bytes).unwrap();
        TargetGraph::read_framed(&tmp)
    }

    #[test]
    fn write_framed_round_trips_distinguishing_counts() {
        let graph = build_distinguishing_graph();

        let mut buf = Vec::new();
        graph.write_framed(&mut buf).unwrap();

        let loaded = read_framed_bytes(&buf).unwrap();

        assert_eq!(loaded.targets_len(), 1);
        assert_eq!(loaded.rule_types_len(), 2);
        assert_eq!(loaded.oncalls_len(), 3);
        assert_eq!(loaded.labels_len(), 4);
        assert_eq!(loaded.glob_patterns_len(), 5);
        assert_eq!(loaded.packages_len(), 6);
        assert_eq!(loaded.files_len(), 7);
        assert_eq!(loaded.ci_deps_patterns_len(), 8);
    }

    #[test]
    fn read_can_be_cancelled() {
        let graph = build_distinguishing_graph();
        let mut file = tempfile::tempfile().unwrap();
        graph.write_framed(&mut file).unwrap();
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let result = TargetGraph::read_framed_cancellable(&file, &|| {
            if checks.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 0 {
                anyhow::bail!("cancelled by reader owner");
            }
            Ok(())
        });
        let err = result.expect_err("cancelled read fails");
        assert!(
            format!("{err:#}").contains("cancelled by reader owner"),
            "{err:#}"
        );
        let loaded = TargetGraph::read_framed(&file).unwrap();
        assert_eq!(loaded.targets_len(), graph.targets_len());
        assert_eq!(loaded.files_len(), graph.files_len());
    }

    #[test]
    fn write_framed_round_trips_through_tempfile() {
        use std::fs::OpenOptions;
        use std::io::BufWriter;

        let graph = build_distinguishing_graph();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.bin");

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let mut writer = BufWriter::new(file);
        graph.write_framed(&mut writer).unwrap();
        std::io::Write::flush(&mut writer).unwrap();
        drop(writer);

        let file = std::fs::File::open(&path).unwrap();
        let loaded = TargetGraph::read_framed(&file).unwrap();

        assert_eq!(loaded.targets_len(), 1);
        assert_eq!(loaded.minimized_targets_len(), 0);
        assert_eq!(loaded.ci_deps_patterns_len(), 8);
    }

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
    fn stored_target_hash_is_a_json_string() {
        let graph = TargetGraph::new();
        let target = graph.store_target("//pkg:target");
        let minimized = minimized_with_labels(&graph, vec![]);
        let hash_text = minimized.target_hash.to_string();
        graph.store_minimized_target(target, minimized);
        let stored = graph.minimized_targets.get(&target).unwrap();

        let json = serde_json::to_value(&*stored).unwrap();
        assert_eq!(json["target_hash"], hash_text);
        assert_eq!(
            serde_json::from_value::<StoredMinimizedTarget>(json).unwrap(),
            *stored
        );
    }

    #[rstest]
    #[case::schema_10(10)]
    #[case::schema_11(11)]
    #[case::previous_schema(SCHEMA_VERSION - 1)]
    fn graph_cache_rejects_prior_schemas(#[case] version: u32) {
        let graph = TargetGraph::new();
        let mut bytes = Vec::new();
        graph.write_framed(&mut bytes).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            SCHEMA_VERSION
        );
        bytes[4..8].copy_from_slice(&version.to_le_bytes());
        let error = read_framed_bytes(&bytes).unwrap_err();
        assert!(error.to_string().contains("framed file version"), "{error}");
    }

    #[test]
    fn graph_cache_hashes_round_trip() {
        let graph = TargetGraph::new();
        let hashes = [
            "5700a84a628259e252ef6952d6af6079",
            "5700A84A628259E252EF6952D6AF6079",
            "5700a84A628259e252EF6952d6af6079",
            "abc",
            "",
            "é東京",
        ];
        let targets: Vec<_> = hashes
            .iter()
            .enumerate()
            .map(|(index, hash)| {
                let target = graph.store_target(&format!("//pkg:t{index}"));
                let mut minimized = minimized_with_labels(&graph, vec![]);
                minimized.target_hash = TargetHash::new(hash);
                graph.store_minimized_target(target, minimized);
                target
            })
            .collect();

        let mut bytes = Vec::new();
        graph.write_framed(&mut bytes).unwrap();
        let loaded = read_framed_bytes(&bytes).unwrap();
        for (target, hash) in targets.into_iter().zip(hashes) {
            let minimized = loaded.get_minimized_target(target).unwrap();
            assert_eq!(minimized.target_hash, TargetHash::new(hash));
            assert_eq!(minimized.target_hash.to_string(), hash);
        }
    }

    fn minimized_with_labels(graph: &TargetGraph, labels: Vec<LabelId>) -> MinimizedBuckTarget {
        MinimizedBuckTarget {
            rule_type: graph.store_rule_type("cpp_library"),
            oncall: None,
            labels,
            target_hash: TargetHash::new("5700a84a628259e252ef6952d6af6079"),
        }
    }

    /// Targets whose labels are equal in the same order share one interned
    /// sequence; a different order, and the empty sequence, are their own.
    #[test]
    fn label_sets_are_interned_by_content_and_order() {
        let graph = TargetGraph::new();
        let a = graph.store_label("ci_test");
        let b = graph.store_label("production");
        let targets: Vec<TargetId> = (0..4)
            .map(|i| graph.store_target(&format!("//pkg:t{i}")))
            .collect();

        graph.store_minimized_target(targets[0], minimized_with_labels(&graph, vec![a, b]));
        graph.store_minimized_target(targets[1], minimized_with_labels(&graph, vec![a, b]));
        assert_eq!(graph.label_sets_len(), 1, "same labels, same order");
        graph.store_minimized_target(targets[2], minimized_with_labels(&graph, vec![b, a]));
        assert_eq!(graph.label_sets_len(), 2, "same labels, other order");
        graph.store_minimized_target(targets[3], minimized_with_labels(&graph, vec![]));
        assert_eq!(graph.label_sets_len(), 3, "no labels");

        for (target, labels) in targets
            .iter()
            .zip([vec![a, b], vec![a, b], vec![b, a], vec![]])
        {
            assert_eq!(graph.get_minimized_target(*target).unwrap().labels, labels);
        }
    }

    #[test]
    fn label_sets_survive_removal_replacement_and_incremental_rewrite() {
        let graph = TargetGraph::new();
        let a = graph.store_label("ci_test");
        let b = graph.store_label("production");
        let first = graph.store_target("//pkg:first");
        let second = graph.store_target("//pkg:second");
        let original = minimized_with_labels(&graph, vec![a, b]);
        let replacement = minimized_with_labels(&graph, vec![b, a, a]);
        graph.store_minimized_target(first, original.clone());
        graph.store_minimized_target(second, original.clone());

        graph.remove_target(first);
        assert_eq!(graph.get_minimized_target(first), None);
        assert_eq!(graph.get_minimized_target(second), Some(original.clone()));
        graph.store_minimized_target(second, replacement.clone());
        assert_eq!(graph.store_target("//pkg:first"), first);
        graph.store_minimized_target(first, original.clone());

        let mut initial = Vec::new();
        graph.write_framed(&mut initial).unwrap();
        let loaded = read_framed_bytes(&initial).unwrap();
        assert_eq!(loaded.get_minimized_target(first), Some(original.clone()));
        assert_eq!(loaded.get_minimized_target(second), Some(replacement));

        loaded.remove_targets_batch(&[second].into_iter().collect());
        assert_eq!(loaded.get_minimized_target(second), None);
        assert_eq!(loaded.get_minimized_target(first), Some(original.clone()));
        let mut rewritten = Vec::new();
        loaded.write_framed(&mut rewritten).unwrap();
        let reloaded = read_framed_bytes(&rewritten).unwrap();
        assert_eq!(reloaded.get_minimized_target(first), Some(original));
        assert_eq!(reloaded.get_minimized_target(second), None);
    }

    #[test]
    fn target_has_label_reads_through_the_interned_set() {
        let graph = TargetGraph::new();
        let carried = graph.store_label("ci_test");
        let other = graph.store_label("production");
        let target = graph.store_target("//pkg:t");
        graph.store_minimized_target(target, minimized_with_labels(&graph, vec![carried]));

        assert!(graph.target_has_label(target, carried));
        assert!(!graph.target_has_label(target, other));
        assert!(!graph.target_has_label(graph.store_target("//pkg:unknown"), carried));
    }

    #[test]
    fn iter_targets_with_any_label_returns_every_target_carrying_one() {
        let graph = TargetGraph::new();
        let a = graph.store_label("a");
        let b = graph.store_label("b");
        let c = graph.store_label("c");
        let targets: Vec<TargetId> = (0..4)
            .map(|i| graph.store_target(&format!("//pkg:t{i}")))
            .collect();
        for (target, labels) in targets.iter().zip([vec![a, b], vec![b], vec![c], vec![]]) {
            graph.store_minimized_target(*target, minimized_with_labels(&graph, labels));
        }

        let wanted: IdHashSet<LabelId> = [a, c].into_iter().collect();
        let found: IdHashSet<TargetId> = graph.iter_targets_with_any_label(&wanted).collect();
        assert_eq!(found, [targets[0], targets[2]].into_iter().collect());
    }

    /// Label sets survive a write and read, including two sets that hold the
    /// same labels in a different order.
    #[test]
    fn label_sets_round_trip() {
        let graph = TargetGraph::new();
        let a = graph.store_label("ci_test");
        let b = graph.store_label("production");
        let targets: Vec<TargetId> = (0..8)
            .map(|i| graph.store_target(&format!("//pkg:t{i}")))
            .collect();
        let label_sets = [vec![a, b], vec![b, a], vec![a], vec![]];
        for (i, target) in targets.iter().enumerate() {
            let labels = label_sets[i % label_sets.len()].clone();
            graph.store_minimized_target(*target, minimized_with_labels(&graph, labels));
        }

        let mut buf = Vec::new();
        graph.write_framed(&mut buf).unwrap();
        let loaded = read_framed_bytes(&buf).unwrap();

        assert_eq!(loaded.label_sets_len(), label_sets.len());
        for target in &targets {
            assert_eq!(
                loaded.get_minimized_target(*target),
                graph.get_minimized_target(*target)
            );
        }
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
    fn set_deps_replaces_existing_deps() {
        let graph = TargetGraph::new();
        let t = graph.store_target("fbcode//a:t");
        let d1 = graph.store_target("fbcode//b:d1");
        let d2 = graph.store_target("fbcode//c:d2");
        let d3 = graph.store_target("fbcode//d:d3");

        graph.set_deps(t, vec![d1, d2]);
        assert_eq!(graph.get_deps(t).unwrap(), vec![d1, d2]);

        graph.set_deps(t, vec![d3]);
        assert_eq!(graph.get_deps(t).unwrap(), vec![d3]);
    }

    #[test]
    fn set_deps_with_empty_removes_entry() {
        let graph = TargetGraph::new();
        let t = graph.store_target("fbcode//a:t");
        let d1 = graph.store_target("fbcode//b:d1");

        graph.set_deps(t, vec![d1]);
        assert!(graph.get_deps(t).is_some());

        graph.set_deps(t, vec![]);
        assert!(graph.get_deps(t).is_none());
    }

    #[test]
    fn add_to_rdeps_only_modifies_rdeps() {
        let graph = TargetGraph::new();
        let dep = graph.store_target("fbcode//a:dep");
        let dependent = graph.store_target("fbcode//b:dependent");

        graph.add_to_rdeps(dep, dependent);

        assert_eq!(graph.get_rdeps(dep).unwrap(), vec![dependent]);
        assert!(
            graph.get_deps(dependent).is_none(),
            "add_to_rdeps should not modify deps"
        );
    }

    #[test]
    fn remove_from_rdeps_removes_target_and_cleans_empty() {
        let graph = TargetGraph::new();
        let dep = graph.store_target("fbcode//a:dep");
        let d1 = graph.store_target("fbcode//b:d1");
        let d2 = graph.store_target("fbcode//c:d2");

        graph.add_to_rdeps(dep, d1);
        graph.add_to_rdeps(dep, d2);
        assert_eq!(graph.get_rdeps(dep).unwrap().len(), 2);

        graph.remove_from_rdeps(dep, d1);
        assert_eq!(graph.get_rdeps(dep).unwrap(), vec![d2]);

        graph.remove_from_rdeps(dep, d2);
        assert!(
            graph.get_rdeps(dep).is_none(),
            "should remove empty rdeps entry"
        );
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
        graph.add_ci_hint_edge(ci_hint_id, dest_id);
        (ci_hint_id, dest_id)
    }

    #[test]
    fn remove_ci_hint_target_cleans_ci_hint_edges() {
        let graph = TargetGraph::new();
        let (ci_hint_id, dest_id) = setup_ci_hint_edge(&graph);

        graph.remove_target(ci_hint_id);

        assert!(graph.get_ci_hint_affected(ci_hint_id).is_none());
        assert!(graph.get_affecting_ci_hints(dest_id).is_none());
    }

    #[test]
    fn remove_regular_target_preserves_ci_hint_edges() {
        let graph = TargetGraph::new();
        let (ci_hint_id, dest_id) = setup_ci_hint_edge(&graph);

        graph.remove_target(dest_id);

        // CI hint edge is left dangling — same pattern as regular rdeps
        assert_eq!(
            graph.get_ci_hint_affected(ci_hint_id).unwrap(),
            vec![dest_id]
        );
        assert_eq!(
            graph.get_affecting_ci_hints(dest_id).unwrap(),
            vec![ci_hint_id]
        );
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

    #[rstest]
    #[case::both_rdeps_and_ci_hint(true, true, 2)]
    #[case::rdeps_only(true, false, 1)]
    #[case::ci_hint_only(false, true, 1)]
    #[case::neither(false, false, 0)]
    fn get_all_dependents_merges_sources(
        #[case] add_rdep_edge: bool,
        #[case] add_ci_hint: bool,
        #[case] expected_count: usize,
    ) {
        let graph = TargetGraph::new();
        let target = graph.store_target("fbcode//foo:target");
        let regular_dep = graph.store_target("fbcode//foo:regular");
        let ci_affected = graph.store_target("fbcode//foo:ci_affected");

        if add_rdep_edge {
            graph.add_rdep(target, regular_dep);
        }
        if add_ci_hint {
            graph.add_ci_hint_edge(target, ci_affected);
        }

        let result = graph.get_all_dependents(target);

        if expected_count == 0 {
            assert!(result.is_none());
        } else {
            let all = result.unwrap();
            assert_eq!(all.len(), expected_count);
            if add_rdep_edge {
                assert!(all.contains(&regular_dep));
            }
            if add_ci_hint {
                assert!(all.contains(&ci_affected));
            }
        }
    }

    #[test]
    fn ci_hint_edge_get_returns_none_for_unknown() {
        let graph = TargetGraph::new();
        let unknown = graph.store_target("fbcode//foo:unknown");

        assert!(graph.get_ci_hint_affected(unknown).is_none());
        assert!(graph.get_affecting_ci_hints(unknown).is_none());
    }

    #[test]
    fn get_rule_type_file_extracts_bzl_path() {
        let graph = TargetGraph::new();
        let rule_id = graph.store_rule_type("prelude//rules.bzl:python_library");
        assert_eq!(
            graph.get_rule_type_file(rule_id),
            Some("prelude//rules.bzl".to_string())
        );
    }

    #[test]
    fn get_rule_type_file_returns_none_without_colon() {
        let graph = TargetGraph::new();
        let rule_id = graph.store_rule_type("python_library");
        assert!(graph.get_rule_type_file(rule_id).is_none());
    }

    #[test]
    fn is_ci_hint_works_with_full_rule_type() {
        let graph = TargetGraph::new();
        let target_id =
            store_target_with_rule_type(&graph, "fbcode//foo:target", "some//path.bzl:ci_hint");
        assert!(graph.is_ci_hint_target(target_id));
    }

    #[test]
    fn is_ci_hint_works_with_short_rule_type() {
        let graph = TargetGraph::new();
        let target_id =
            store_target_with_rule_type(&graph, "fbcode//foo:target", CI_HINT_RULE_TYPE);
        assert!(graph.is_ci_hint_target(target_id));
    }

    struct FileDepGraphBuilder {
        graph: TargetGraph,
    }

    impl FileDepGraphBuilder {
        fn new() -> Self {
            Self {
                graph: TargetGraph::new(),
            }
        }

        fn add_import(self, importer: &str, imported: &str) -> Self {
            let importer_id = self.graph.store_file(importer);
            let imported_id = self.graph.store_file(imported);
            self.graph.add_file_import(imported_id, importer_id);
            self
        }

        fn build(self) -> TargetGraph {
            self.graph
        }
    }

    #[test]
    fn add_file_import_maintains_forward_and_reverse_maps() {
        let graph = FileDepGraphBuilder::new()
            .add_import("fbcode//pkg/TARGETS", "fbcode//defs.bzl")
            .add_import("fbcode//pkg/TARGETS", "fbcode//utils.bzl")
            .build();

        let targets_file = graph.store_file("fbcode//pkg/TARGETS");
        let defs_bzl = graph.store_file("fbcode//defs.bzl");
        let utils_bzl = graph.store_file("fbcode//utils.bzl");

        assert!(graph.contains_file(defs_bzl));
        assert!(!graph.contains_file("fbcode//missing.bzl".parse().unwrap()));
        assert_eq!(
            graph.with_file_path(defs_bzl, str::to_owned),
            Some("fbcode//defs.bzl".to_owned())
        );
        assert_eq!(graph.get_file_rdeps(defs_bzl).unwrap(), vec![targets_file]);
        assert_eq!(graph.get_file_rdeps(utils_bzl).unwrap(), vec![targets_file]);

        let deps = graph.get_file_deps(targets_file).unwrap();
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&defs_bzl));
        assert!(deps.contains(&utils_bzl));
    }

    #[test]
    fn get_file_deps_returns_none_for_file_with_no_imports() {
        let graph = TargetGraph::new();
        let file = graph.store_file("fbcode//standalone.bzl");
        assert!(graph.get_file_deps(file).is_none());
    }

    #[rstest]
    #[case::cleans_all_entries(
        &[("fbcode//pkg/TARGETS", "fbcode//defs.bzl"), ("fbcode//pkg/TARGETS", "fbcode//utils.bzl")],
        "fbcode//pkg/TARGETS",
        &[("fbcode//pkg/TARGETS", None), ("fbcode//defs.bzl", None), ("fbcode//utils.bzl", None)],
    )]
    #[case::noop_for_unknown_file(
        &[("fbcode//pkg/TARGETS", "fbcode//defs.bzl")],
        "fbcode//unknown.bzl",
        &[("fbcode//defs.bzl", Some(vec!["fbcode//pkg/TARGETS"])), ("fbcode//pkg/TARGETS", None)],
    )]
    #[case::only_removes_specified_dependent(
        &[("fbcode//a/TARGETS", "fbcode//defs.bzl"), ("fbcode//b/TARGETS", "fbcode//defs.bzl")],
        "fbcode//a/TARGETS",
        &[("fbcode//defs.bzl", Some(vec!["fbcode//b/TARGETS"]))],
    )]
    #[test]
    fn clear_file_deps_cases(
        #[case] imports: &[(&str, &str)],
        #[case] remove_target: &str,
        #[case] expected_rdeps: &[(&str, Option<Vec<&str>>)],
    ) {
        let mut builder = FileDepGraphBuilder::new();
        for (importer, imported) in imports {
            builder = builder.add_import(importer, imported);
        }
        let graph = builder.build();

        let remove_id = graph.store_file(remove_target);
        graph.clear_file_deps(remove_id);

        assert!(
            graph.get_file_deps(remove_id).is_none(),
            "cleared file should have no deps"
        );

        for (file, expected) in expected_rdeps {
            let file_id = graph.store_file(file);
            match expected {
                None => {
                    assert!(
                        graph.get_file_rdeps(file_id).is_none(),
                        "{} should have no rdeps after removal",
                        file
                    );
                }
                Some(expected_rdep_files) => {
                    let rdeps = graph
                        .get_file_rdeps(file_id)
                        .unwrap_or_else(|| panic!("{} should have rdeps", file));
                    let expected_ids: Vec<FileId> = expected_rdep_files
                        .iter()
                        .map(|f| graph.store_file(f))
                        .collect();
                    assert_eq!(rdeps, expected_ids, "rdeps mismatch for {}", file);
                }
            }
        }
    }

    fn store_minimized_stub(graph: &TargetGraph, target_id: TargetId) {
        let rule_type_id = graph.store_rule_type("cpp_library");
        graph.store_minimized_target(
            target_id,
            MinimizedBuckTarget {
                rule_type: rule_type_id,
                oncall: None,
                labels: vec![],
                target_hash: TargetHash::new("hash"),
            },
        );
    }

    #[test]
    fn batch_remove_cleans_rdeps_same_as_individual_removal() {
        let graph = TargetGraph::new();

        let shared_dep = graph.store_target("fbcode//lib:shared");
        let target_a = graph.store_target("fbcode//a:target_a");
        let target_b = graph.store_target("fbcode//b:target_b");
        let survivor = graph.store_target("fbcode//c:survivor");

        graph.add_rdep(shared_dep, target_a);
        graph.add_rdep(shared_dep, target_b);
        graph.add_rdep(shared_dep, survivor);

        for &id in &[target_a, target_b, shared_dep, survivor] {
            store_minimized_stub(&graph, id);
        }

        let to_remove: IdHashSet<TargetId> = [target_a, target_b].into_iter().collect();
        graph.remove_targets_batch(&to_remove);

        assert_eq!(graph.get_rdeps(shared_dep).unwrap(), vec![survivor]);
        assert!(graph.get_minimized_target(target_a).is_none());
        assert!(graph.get_minimized_target(target_b).is_none());
        assert!(graph.get_deps(target_a).is_none());
        assert!(graph.get_deps(target_b).is_none());
        assert!(graph.get_minimized_target(shared_dep).is_some());
        assert!(graph.get_minimized_target(survivor).is_some());
    }

    #[test]
    fn batch_remove_handles_ci_hint_targets() {
        let graph = TargetGraph::new();

        let ci_hint_id =
            store_target_with_rule_type(&graph, "fbcode//foo:ci_hint@test", CI_HINT_RULE_TYPE);
        let dest_id = store_target_with_rule_type(&graph, "fbcode//foo:test", "python_test");
        graph.add_ci_hint_edge(ci_hint_id, dest_id);

        let to_remove: IdHashSet<TargetId> = [ci_hint_id].into_iter().collect();
        graph.remove_targets_batch(&to_remove);

        assert!(graph.get_ci_hint_affected(ci_hint_id).is_none());
        assert!(graph.get_affecting_ci_hints(dest_id).is_none());
        assert!(graph.get_minimized_target(ci_hint_id).is_none());
    }

    #[test]
    fn batch_remove_cleans_dep_rdeps_for_targets_with_different_deps() {
        let graph = TargetGraph::new();

        let dep_x = graph.store_target("fbcode//lib:dep_x");
        let dep_y = graph.store_target("fbcode//lib:dep_y");
        let target_a = graph.store_target("fbcode//a:target_a");
        let target_b = graph.store_target("fbcode//b:target_b");

        graph.add_rdep(dep_x, target_a);
        graph.add_rdep(dep_y, target_b);

        for &id in &[dep_x, dep_y, target_a, target_b] {
            store_minimized_stub(&graph, id);
        }

        let to_remove: IdHashSet<TargetId> = [target_a, target_b].into_iter().collect();
        graph.remove_targets_batch(&to_remove);

        assert!(graph.get_rdeps(dep_x).is_none());
        assert!(graph.get_rdeps(dep_y).is_none());
    }

    #[test]
    fn batch_remove_empty_set_is_noop() {
        let graph = TargetGraph::new();

        let target = graph.store_target("fbcode//a:target");
        store_minimized_stub(&graph, target);

        let to_remove: IdHashSet<TargetId> = IdHashSet::default();
        graph.remove_targets_batch(&to_remove);

        assert!(graph.get_minimized_target(target).is_some());
    }

    #[test]
    fn batch_remove_cleans_package_id_to_targets() {
        let graph = TargetGraph::new();

        let package_id = graph.store_package("fbcode//pkg");
        let target_a = graph.store_target("fbcode//pkg:a");
        let target_b = graph.store_target("fbcode//pkg:b");
        let survivor = graph.store_target("fbcode//pkg:survivor");

        graph.add_target_to_package(package_id, target_a);
        graph.add_target_to_package(package_id, target_b);
        graph.add_target_to_package(package_id, survivor);

        for &id in &[target_a, target_b, survivor] {
            store_minimized_stub(&graph, id);
        }

        let to_remove: IdHashSet<TargetId> = [target_a, target_b].into_iter().collect();
        graph.remove_targets_batch(&to_remove);

        let remaining = graph.get_targets_in_package(package_id).unwrap();
        assert_eq!(remaining, vec![survivor]);
    }

    #[test]
    fn remove_finds_target_filed_under_another_package() {
        let graph = TargetGraph::new();

        // Filed under a package its label does not name: removal must fall
        // back to scanning instead of leaving it behind.
        let other = graph.store_package("fbcode//other");
        let misfiled = graph.store_target("fbcode//pkg:misfiled");
        let single = graph.store_target("fbcode//pkg:single");
        let kept = graph.store_target("fbcode//other:kept");
        graph.add_target_to_package(other, misfiled);
        graph.add_target_to_package(other, single);
        graph.add_target_to_package(other, kept);
        for &id in &[misfiled, single, kept] {
            store_minimized_stub(&graph, id);
        }

        graph.remove_targets_batch(&[misfiled].into_iter().collect());
        graph.remove_target(single);

        assert_eq!(graph.get_targets_in_package(other).unwrap(), vec![kept]);
    }

    #[test]
    fn batch_remove_cleans_package_id_to_targets_across_packages() {
        let graph = TargetGraph::new();

        let pkg1 = graph.store_package("fbcode//pkg1");
        let pkg2 = graph.store_package("fbcode//pkg2");
        let target_a = graph.store_target("fbcode//pkg1:a");
        let target_b = graph.store_target("fbcode//pkg2:b");
        let survivor = graph.store_target("fbcode//pkg2:survivor");

        graph.add_target_to_package(pkg1, target_a);
        graph.add_target_to_package(pkg2, target_b);
        graph.add_target_to_package(pkg2, survivor);

        for &id in &[target_a, target_b, survivor] {
            store_minimized_stub(&graph, id);
        }

        let to_remove: IdHashSet<TargetId> = [target_a, target_b].into_iter().collect();
        graph.remove_targets_batch(&to_remove);

        assert!(graph.get_targets_in_package(pkg1).is_none());
        let remaining = graph.get_targets_in_package(pkg2).unwrap();
        assert_eq!(remaining, vec![survivor]);
    }

    #[test]
    fn remove_target_cleans_package_id_to_targets() {
        let graph = TargetGraph::new();

        let package_id = graph.store_package("fbcode//pkg");
        let target_a = graph.store_target("fbcode//pkg:a");
        let target_b = graph.store_target("fbcode//pkg:b");

        graph.add_target_to_package(package_id, target_a);
        graph.add_target_to_package(package_id, target_b);

        for &id in &[target_a, target_b] {
            store_minimized_stub(&graph, id);
        }

        graph.remove_target(target_a);

        let remaining = graph.get_targets_in_package(package_id).unwrap();
        assert_eq!(remaining, vec![target_b]);
    }

    #[test]
    fn clear_errors_removes_all_errors_for_package() {
        let graph = TargetGraph::new();
        let package_id = graph.store_package("fbcode//broken/pkg");
        graph.add_error(package_id, "error one".to_owned());
        graph.add_error(package_id, "error two".to_owned());
        assert_eq!(graph.get_errors(package_id).unwrap().len(), 2);

        graph.clear_errors(package_id);

        assert!(graph.get_errors(package_id).is_none());
    }

    #[test]
    fn clear_errors_is_noop_for_package_without_errors() {
        let graph = TargetGraph::new();
        let package_id = graph.store_package("fbcode//clean/pkg");

        graph.clear_errors(package_id);

        assert!(graph.get_errors(package_id).is_none());
    }

    #[test]
    fn error_package_ids_returns_packages_with_errors() {
        let graph = TargetGraph::new();
        let pkg_a = graph.store_package("fbcode//a");
        let pkg_b = graph.store_package("fbcode//b");
        let _pkg_c = graph.store_package("fbcode//c");

        graph.add_error(pkg_a, "err".to_owned());
        graph.add_error(pkg_b, "err".to_owned());

        let ids = graph.error_package_ids();

        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&pkg_a));
        assert!(ids.contains(&pkg_b));
    }

    fn rdep_map(entries: &[(TargetId, TargetId)]) -> IdDashMap<TargetId, Vec<TargetId>> {
        let map: IdDashMap<TargetId, Vec<TargetId>> = IdDashMap::default();
        for &(dep, target) in entries {
            map.entry(dep).or_default().push(target);
        }
        map
    }

    #[test]
    fn batch_update_rdeps_applies_additions_and_removals() {
        let graph = TargetGraph::new();
        let dep1 = graph.store_target("fbcode//a:dep1");
        let dep2 = graph.store_target("fbcode//a:dep2");
        let t1 = graph.store_target("fbcode//b:t1");
        let t2 = graph.store_target("fbcode//b:t2");
        let t3 = graph.store_target("fbcode//b:t3");

        graph.add_to_rdeps(dep1, t1);
        graph.add_to_rdeps(dep1, t2);
        graph.add_to_rdeps(dep2, t1);

        graph.batch_update_rdeps(rdep_map(&[(dep1, t3), (dep2, t2)]), rdep_map(&[(dep1, t1)]));

        let rdeps1 = graph.get_rdeps(dep1).unwrap();
        assert_eq!(rdeps1.len(), 2);
        assert!(rdeps1.contains(&t2));
        assert!(rdeps1.contains(&t3));
        assert!(!rdeps1.contains(&t1));

        let rdeps2 = graph.get_rdeps(dep2).unwrap();
        assert_eq!(rdeps2.len(), 2);
        assert!(rdeps2.contains(&t1));
        assert!(rdeps2.contains(&t2));
    }

    #[test]
    fn batch_update_rdeps_cleans_empty_entries() {
        let graph = TargetGraph::new();
        let dep = graph.store_target("fbcode//a:dep");
        let t1 = graph.store_target("fbcode//b:t1");

        graph.add_to_rdeps(dep, t1);

        graph.batch_update_rdeps(rdep_map(&[]), rdep_map(&[(dep, t1)]));

        assert!(
            graph.get_rdeps(dep).is_none(),
            "should remove empty rdeps entry"
        );
    }

    #[test]
    fn batch_update_rdeps_additions_to_nonexistent() {
        let graph = TargetGraph::new();
        let dep = graph.store_target("fbcode//a:dep");
        let t1 = graph.store_target("fbcode//b:t1");

        graph.batch_update_rdeps(rdep_map(&[(dep, t1)]), rdep_map(&[]));

        assert_eq!(graph.get_rdeps(dep).unwrap(), vec![t1]);
    }

    #[rstest]
    #[case::empty(0, 1)]
    #[case::tiny(1, 1)]
    #[case::just_under(1_499_999, 1)]
    #[case::single_shard_boundary(1_500_000, 1)]
    #[case::just_over(1_500_001, 2)]
    #[case::four_shards(6_000_000, 4)]
    #[case::eight_shards(12_000_000, 8)]
    #[case::clamps_to_max(1_000_000_000, 16)]
    fn pick_shard_count_picks_within_bounds(#[case] entries: usize, #[case] expected: usize) {
        assert_eq!(pick_shard_count(entries), expected);
    }

    #[rstest]
    #[case(1)]
    #[case(4)]
    #[case(16)]
    fn shard_index_stays_in_bounds(#[case] shard_count: usize) {
        for raw_id in 0..1000u64 {
            let target_id = TargetId(raw_id);
            assert!(target_id.shard_index(shard_count) < shard_count);
        }
    }

    #[rstest]
    #[case(1)]
    #[case(4)]
    #[case(8)]
    fn sharded_field_routes_inserts_to_id_modulo_shard(#[case] shard_count: usize) {
        let field: ShardedField<IdDashMap<TargetId, String>> = ShardedField::new(shard_count);
        for raw_id in 0..200u64 {
            let target_id = TargetId(raw_id);
            field.insert(target_id, format!("v{raw_id}"));
        }

        for raw_id in 0..200u64 {
            let target_id = TargetId(raw_id);
            let expected_shard = target_id.shard_index(shard_count);
            assert!(
                field.shards()[expected_shard].contains_key(&target_id),
                "id {raw_id} should be in shard {expected_shard}",
            );
            for (other_shard, shard) in field.shards().iter().enumerate() {
                if other_shard != expected_shard {
                    assert!(
                        !shard.contains_key(&target_id),
                        "id {raw_id} should NOT be in shard {other_shard}",
                    );
                }
            }
        }
    }

    #[rstest]
    #[case(1)]
    #[case(4)]
    #[case(8)]
    fn sharded_field_get_returns_inserted_values(#[case] shard_count: usize) {
        let field: ShardedField<IdDashMap<TargetId, String>> = ShardedField::new(shard_count);
        for raw_id in 0..50u64 {
            field.insert(TargetId(raw_id), format!("v{raw_id}"));
        }

        for raw_id in 0..50u64 {
            assert_eq!(
                field.get(&TargetId(raw_id)).map(|v| v.clone()),
                Some(format!("v{raw_id}"))
            );
        }
        assert_eq!(field.len(), 50);
        assert!(!field.is_empty());
    }

    #[rstest]
    #[case(1)]
    #[case(4)]
    #[case(8)]
    fn sharded_field_round_trips_through_json(#[case] shard_count: usize) {
        let original: ShardedField<IdDashMap<TargetId, String>> = ShardedField::new(shard_count);
        for raw_id in 0..200u64 {
            original.insert(TargetId(raw_id), format!("v{raw_id}"));
        }

        let encoded = serde_json::to_vec(&original).unwrap();
        let decoded: ShardedField<IdDashMap<TargetId, String>> =
            serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded.shards().len(), 1);
        assert_eq!(decoded.len(), 200);
        for raw_id in 0..200u64 {
            assert_eq!(
                decoded.get(&TargetId(raw_id)).map(|v| v.clone()),
                Some(format!("v{raw_id}"))
            );
        }
    }

    #[test]
    fn sharded_field_new_with_zero_shards_panics() {
        let result = std::panic::catch_unwind(|| {
            let _: ShardedField<IdDashMap<TargetId, String>> = ShardedField::new(0);
        });
        assert!(result.is_err());
    }
}
