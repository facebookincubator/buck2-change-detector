/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::borrow::Cow;
use std::marker::PhantomData;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use dashmap::mapref::entry::VacantEntry;
use dashmap::mapref::multiple::RefMulti;
use dashmap::mapref::one::Ref;
use dashmap::mapref::one::RefMut;
use rayon::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use super::super::CheckCancelled;
use super::super::IdDashMap;
use super::Column;
use super::INDEX_BYTES;
use super::Key;
use super::Value;
use super::View;
use super::WORD_BYTES;

const OVERRIDE_WORD_BITS: usize = u64::BITS as usize;

#[derive(Debug)]
pub(in super::super) struct Map<K: Key, V: Value> {
    base: Option<Column>,
    // None is a tombstone only for a base row. Per-key changes are serialized
    // by this map; publication bits let other base rows remain lock-free.
    // Reuse DashMap instantiations across ID types; the field API stays typed.
    changes: IdDashMap<u64, Option<V>>,
    overridden: Vec<AtomicU64>,
    loaded_len: AtomicUsize,
    key_type: PhantomData<K>,
}
impl<K: Key, V: Value> Default for Map<K, V> {
    fn default() -> Self {
        Self {
            base: None,
            changes: IdDashMap::default(),
            overridden: Vec::new(),
            loaded_len: AtomicUsize::new(0),
            key_type: PhantomData,
        }
    }
}

enum Source<'a, V: Value> {
    One(Ref<'a, u64, Option<V>>),
    Many(RefMulti<'a, u64, Option<V>>),
    Native(View<'a>),
}
pub(in super::super) struct ReadRef<'a, K: Key, V: Value> {
    key: K,
    source: Source<'a, V>,
}
impl<'a, K: Key, V: Value> ReadRef<'a, K, V> {
    fn native(key: K, bytes: View<'a>) -> Self {
        Self {
            key,
            source: Source::Native(bytes),
        }
    }
    pub(in super::super) fn key(&self) -> &K {
        &self.key
    }
    /// Borrows changed rows, or materializes a native row for this call.
    /// Prefer text/ID views when the owned representation is not needed.
    pub(in super::super) fn value(&self) -> Cow<'_, V> {
        match &self.source {
            Source::Native(bytes) => Cow::Owned(V::decode_view(*bytes)),
            _ => Cow::Borrowed(self.changed_value()),
        }
    }
    pub(in super::super) fn to_owned_value(&self) -> V {
        self.value().into_owned()
    }
    fn changed_value(&self) -> &V {
        match &self.source {
            Source::One(r) => r.value().as_ref().expect("live changed row"),
            Source::Many(r) => r.value().as_ref().expect("live changed row"),
            Source::Native(_) => unreachable!("native rows use their packed view"),
        }
    }
}
impl<K: Key> ReadRef<'_, K, String> {
    pub(in super::super) fn text_bytes(&self) -> &[u8] {
        match &self.source {
            Source::Native(bytes) => bytes.bytes,
            _ => self.changed_value().as_bytes(),
        }
    }
    pub(in super::super) fn text(&self) -> &str {
        match &self.source {
            Source::Native(bytes) => std::str::from_utf8(bytes.bytes).expect("validated text"),
            _ => self.changed_value().as_str(),
        }
    }
}
impl<K: Key, I: Key> ReadRef<'_, K, Vec<I>> {
    pub(in super::super) fn ids_len(&self) -> usize {
        match &self.source {
            Source::Native(bytes) => {
                bytes.bytes.len()
                    / if bytes.ids.is_some() {
                        INDEX_BYTES
                    } else {
                        WORD_BYTES
                    }
            }
            _ => self.changed_value().len(),
        }
    }
    pub(in super::super) fn for_each_id(&self, f: impl FnMut(I)) {
        match &self.source {
            Source::Native(bytes) => super::value::view_ids::<I>(*bytes).for_each(f),
            _ => self.changed_value().iter().copied().for_each(f),
        }
    }
    pub(in super::super) fn append_to(&self, out: &mut Vec<I>) {
        match &self.source {
            Source::Native(bytes) => out.extend(super::value::view_ids::<I>(*bytes)),
            _ => out.extend_from_slice(self.changed_value()),
        }
    }
    pub(in super::super) fn contains_id(&self, id: I) -> bool {
        match &self.source {
            Source::Native(bytes) => super::value::view_ids::<I>(*bytes).any(|v| v == id),
            _ => self.changed_value().contains(&id),
        }
    }
    pub(in super::super) fn any_id(&self, f: impl FnMut(I) -> bool) -> bool {
        match &self.source {
            Source::Native(bytes) => super::value::view_ids::<I>(*bytes).any(f),
            _ => self.changed_value().iter().copied().any(f),
        }
    }
    pub(in super::super) fn is_empty(&self) -> bool {
        match &self.source {
            Source::Native(bytes) => bytes.bytes.is_empty(),
            _ => self.changed_value().is_empty(),
        }
    }
}

pub(in super::super) struct Mut<'a, V: Value> {
    inner: RefMut<'a, u64, Option<V>>,
}
impl<V: Value> Deref for Mut<'_, V> {
    type Target = V;
    fn deref(&self) -> &V {
        self.inner.value().as_ref().expect("live changed row")
    }
}
impl<V: Value> DerefMut for Mut<'_, V> {
    fn deref_mut(&mut self) -> &mut V {
        self.inner.value_mut().as_mut().expect("live changed row")
    }
}
pub(in super::super) struct Entry<'a, V: Value> {
    inner: Slot<'a, V>,
    loaded_len: Option<&'a AtomicUsize>,
}
enum Slot<'a, V: Value> {
    Occupied(RefMut<'a, u64, Option<V>>),
    Vacant(VacantEntry<'a, u64, Option<V>>),
}
impl<'a, V: Value> Entry<'a, V> {
    pub(in super::super) fn get(&self) -> Option<&V> {
        match &self.inner {
            Slot::Occupied(inner) => inner.value().as_ref(),
            Slot::Vacant(_) => None,
        }
    }
    fn into_mut(self) -> Option<Mut<'a, V>> {
        match self.inner {
            Slot::Occupied(inner) => inner.is_some().then(|| Mut { inner }),
            Slot::Vacant(_) => None,
        }
    }
    pub(in super::super) fn or_insert_with(self, f: impl FnOnce() -> V) -> Mut<'a, V> {
        let (inner, added) = match self.inner {
            Slot::Occupied(mut inner) => {
                let added = inner.is_none();
                if added {
                    *inner = Some(f());
                }
                (inner, added)
            }
            Slot::Vacant(slot) => (slot.insert(Some(f())), true),
        };
        if let (true, Some(len)) = (added, self.loaded_len) {
            len.fetch_add(1, Ordering::Relaxed);
        }
        Mut { inner }
    }
    pub(in super::super) fn or_insert(self, value: V) -> Mut<'a, V> {
        self.or_insert_with(|| value)
    }
    pub(in super::super) fn or_default(self) -> Mut<'a, V>
    where
        V: Default,
    {
        self.or_insert_with(V::default)
    }
}
/// A removal keeps a native value borrowed unless the caller requests ownership.
enum Removed<'a, V> {
    Native(View<'a>),
    Owned(V),
}
impl<V: Value> Removed<'_, V> {
    fn into_owned(self) -> V {
        match self {
            Self::Native(view) => V::decode_view(view),
            Self::Owned(value) => value,
        }
    }
}

impl<K: Key, V: Value> Map<K, V> {
    fn from_column(column: Column, check: &CheckCancelled<'_>) -> anyhow::Result<Self> {
        column.validate_values::<V>(check)?;
        Ok(Self {
            loaded_len: AtomicUsize::new(column.len()),
            overridden: (0..column.len().div_ceil(OVERRIDE_WORD_BITS))
                .map(|_| AtomicU64::new(0))
                .collect(),
            base: Some(column),
            ..Self::default()
        })
    }
    pub(in super::super) fn len(&self) -> usize {
        if self.base.is_some() {
            self.loaded_len.load(Ordering::Relaxed)
        } else {
            // Fresh rows have no tombstones or vacant placeholders. Reuse the
            // map's shard counts rather than contend on a global counter.
            self.changes.len()
        }
    }

    pub(in super::super) fn from_chunks(
        chunks: Vec<super::Chunk>,
        check: &CheckCancelled<'_>,
    ) -> anyhow::Result<Self> {
        Self::from_column(Column::from_chunks(chunks, check)?, check)
    }

    fn base_row(&self, key: K) -> Option<usize> {
        self.base.as_ref().and_then(|b| b.find(key.raw()))
    }

    /// The position of `key` among the rows the map was loaded with, below
    /// `loaded_positions`, or `None` for a key stored since the load. A
    /// loaded row keeps its position when its value changes or is removed.
    pub(in super::super) fn loaded_position(&self, key: &K) -> Option<usize> {
        self.base_row(*key)
    }

    /// The number of positions `loaded_position` can return.
    pub(in super::super) fn loaded_positions(&self) -> usize {
        self.base.as_ref().map_or(0, |b| b.len())
    }

    /// The key at a loaded position.
    pub(in super::super) fn key_at_loaded_position(&self, position: usize) -> K {
        K::from_raw(
            self.base
                .as_ref()
                .expect("a loaded position implies loaded rows")
                .key(position),
        )
    }

    fn mark_override(&self, row: usize) {
        let bit = 1u64 << (row % OVERRIDE_WORD_BITS);
        let word = &self.overridden[row / OVERRIDE_WORD_BITS];
        if word.load(Ordering::Relaxed) & bit == 0 {
            // Publish before touching the change map. Readers that observe the
            // bit before insertion still return the base value, not absence.
            word.fetch_or(bit, Ordering::Release);
        }
    }

    fn read_base_row(&self, row: usize) -> Option<ReadRef<'_, K, V>> {
        let base = self.base.as_ref().expect("base row");
        let key = K::from_raw(base.key(row));
        if self.overridden[row / OVERRIDE_WORD_BITS].load(Ordering::Acquire)
            & (1 << (row % OVERRIDE_WORD_BITS))
            != 0
        {
            if let Some(r) = self.changes.get(&key.raw()) {
                return r.is_some().then(|| ReadRef {
                    key,
                    source: Source::One(r),
                });
            }
        }
        Some(ReadRef::native(key, base.value(row)))
    }

    fn appended_row<'a>(&'a self, r: RefMulti<'a, u64, Option<V>>) -> Option<ReadRef<'a, K, V>> {
        if r.is_none() || self.base_row(K::from_raw(*r.key())).is_some() {
            return None;
        }
        Some(ReadRef {
            key: K::from_raw(*r.key()),
            source: Source::Many(r),
        })
    }

    pub(in super::super) fn iter(&self) -> impl Iterator<Item = ReadRef<'_, K, V>> {
        let count = self.base.as_ref().map_or(0, Column::len);
        // Immutable base membership partitions the two halves even while the
        // first override is published. Iteration is not a whole-map snapshot.
        (0..count)
            .filter_map(|row| self.read_base_row(row))
            .chain(self.changes.iter().filter_map(|r| self.appended_row(r)))
    }

    pub(in super::super) fn par_iter(&self) -> impl ParallelIterator<Item = ReadRef<'_, K, V>> {
        let count = self.base.as_ref().map_or(0, Column::len);
        (0..count)
            .into_par_iter()
            .filter_map(|row| self.read_base_row(row))
            .chain(self.changes.par_iter().filter_map(|r| self.appended_row(r)))
    }

    pub(in super::super) fn get(&self, key: &K) -> Option<ReadRef<'_, K, V>> {
        if let Some(row) = self.base_row(*key) {
            return self.read_base_row(row);
        }
        let r = self.changes.get(&key.raw())?;
        r.is_some().then(|| ReadRef {
            key: *key,
            source: Source::One(r),
        })
    }

    pub(in super::super) fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    fn entry_at(&self, key: K, row: Option<usize>) -> Entry<'_, V> {
        if let Some(row) = row {
            self.mark_override(row);
        }
        let inner = match self.changes.entry(key.raw()) {
            dashmap::mapref::entry::Entry::Occupied(slot) => Slot::Occupied(slot.into_ref()),
            dashmap::mapref::entry::Entry::Vacant(slot) => match row {
                Some(row) => Slot::Occupied(slot.insert(Some(V::decode_view(
                    self.base.as_ref().expect("base row").value(row),
                )))),
                None => Slot::Vacant(slot),
            },
        };
        Entry {
            inner,
            loaded_len: self.base.as_ref().map(|_| &self.loaded_len),
        }
    }

    pub(in super::super) fn entry(&self, key: K) -> Entry<'_, V> {
        self.entry_at(key, self.base_row(key))
    }

    pub(in super::super) fn get_mut(&self, key: &K) -> Option<Mut<'_, V>> {
        if let Some(row) = self.base_row(*key) {
            self.entry_at(*key, Some(row)).into_mut()
        } else {
            let inner = self.changes.get_mut(&key.raw())?;
            inner.is_some().then(|| Mut { inner })
        }
    }

    pub(in super::super) fn insert(&self, key: K, value: V) -> bool {
        let row = self.base_row(key);
        if let Some(row) = row {
            self.mark_override(row);
        }
        match self.changes.entry(key.raw()) {
            dashmap::mapref::entry::Entry::Occupied(mut slot) => {
                let added = slot.get().is_none();
                slot.insert(Some(value));
                if added && self.base.is_some() {
                    self.loaded_len.fetch_add(1, Ordering::Relaxed);
                }
                added
            }
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                let _guard = slot.insert(Some(value));
                if row.is_none() && self.base.is_some() {
                    self.loaded_len.fetch_add(1, Ordering::Relaxed);
                }
                row.is_none()
            }
        }
    }

    fn take(&self, key: &K) -> Option<Removed<'_, V>> {
        let Some(row) = self.base_row(*key) else {
            return self
                .changes
                .remove_if(&key.raw(), |_, value| {
                    if value.is_some() && self.base.is_some() {
                        self.loaded_len.fetch_sub(1, Ordering::Relaxed);
                    }
                    true
                })
                .and_then(|(_, value)| value.map(Removed::Owned));
        };
        self.mark_override(row);
        match self.changes.entry(key.raw()) {
            dashmap::mapref::entry::Entry::Occupied(mut slot) => {
                let value = slot.get_mut().take()?;
                self.loaded_len.fetch_sub(1, Ordering::Relaxed);
                Some(Removed::Owned(value))
            }
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                let _guard = slot.insert(None);
                self.loaded_len.fetch_sub(1, Ordering::Relaxed);
                Some(Removed::Native(
                    self.base.as_ref().expect("base row").value(row),
                ))
            }
        }
    }

    pub(in super::super) fn remove(&self, key: &K) -> Option<(K, V)> {
        self.take(key).map(|value| (*key, value.into_owned()))
    }

    pub(in super::super) fn discard(&self, key: &K) -> bool {
        self.take(key).is_some()
    }
}
impl<K: Key, I: Key> Map<K, Vec<I>> {
    /// Packed rows have no vector capacity slack. Only owned rows can shrink.
    pub(in super::super) fn shrink_owned_vectors(&self) {
        self.changes.par_iter_mut().for_each(|mut row| {
            if let Some(value) = row.value_mut() {
                value.shrink_to_fit();
            }
        });
    }

    pub(in super::super) fn from_list_chunks(
        chunks: Vec<super::Chunk>,
        ids: std::sync::Arc<Vec<u64>>,
        check: &CheckCancelled<'_>,
    ) -> anyhow::Result<Self> {
        let mut column = Column::from_chunks(chunks, check)?;
        column.ids = Some(ids);
        Self::from_column(column, check)
    }
}
impl<K: Key> Map<K, String> {
    /// Keys identify string contents. Probe loaded maps before entry() to avoid
    /// copying an unchanged base row. Fresh maps avoid a separate lookup, but
    /// their entry() still takes a write lock when the key is already present.
    pub(in super::super) fn intern(&self, key: K, value: &str) {
        if self.base.is_some() && self.contains_key(&key) {
            return;
        }
        self.entry(key).or_insert_with(|| value.to_owned());
    }
}
impl<K: Key, V: Value> Serialize for Map<K, V> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.len()))?;
        for r in self.iter() {
            m.serialize_entry(r.key(), &r.value())?;
        }
        m.end()
    }
}
impl<'de, K: Key, V: Value> Deserialize<'de> for Map<K, V> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct MapVisitor<K, V>(PhantomData<(K, V)>);
        impl<'de, K: Key, V: Value> serde::de::Visitor<'de> for MapVisitor<K, V> {
            type Value = Map<K, V>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a graph field map")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut entries: A,
            ) -> Result<Self::Value, A::Error> {
                let result = Map::default();
                while let Some((key, value)) = entries.next_entry()? {
                    result.insert(key, value);
                }
                Ok(result)
            }
        }
        d.deserialize_map(MapVisitor(PhantomData))
    }
}

#[derive(Debug)]
pub(in super::super) struct Set<K: Key>(Map<K, ()>);
impl<K: Key> Default for Set<K> {
    fn default() -> Self {
        Self(Map::default())
    }
}
pub(in super::super) struct SetRef<'a, K: Key>(ReadRef<'a, K, ()>);
impl<K: Key> Deref for SetRef<'_, K> {
    type Target = K;
    fn deref(&self) -> &K {
        self.0.key()
    }
}
impl<K: Key> Set<K> {
    pub(in super::super) fn from_chunks(
        chunks: Vec<super::Chunk>,
        check: &CheckCancelled<'_>,
    ) -> anyhow::Result<Self> {
        Ok(Self(Map::from_chunks(chunks, check)?))
    }
    pub(in super::super) fn insert(&self, k: K) -> bool {
        self.0.insert(k, ())
    }
    pub(in super::super) fn discard(&self, k: &K) -> bool {
        self.0.discard(k)
    }
    pub(in super::super) fn contains(&self, k: &K) -> bool {
        self.0.contains_key(k)
    }
    pub(in super::super) fn len(&self) -> usize {
        self.0.len()
    }
    pub(in super::super) fn iter(&self) -> impl Iterator<Item = SetRef<'_, K>> {
        self.0.iter().map(SetRef)
    }
}
impl<K: Key> Serialize for Set<K> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(self.len()))?;
        for r in self.iter() {
            seq.serialize_element(&*r)?;
        }
        seq.end()
    }
}
impl<'de, K: Key> Deserialize<'de> for Set<K> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct SetVisitor<K>(PhantomData<K>);
        impl<'de, K: Key> serde::de::Visitor<'de> for SetVisitor<K> {
            type Value = Set<K>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a graph field set")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut entries: A,
            ) -> Result<Self::Value, A::Error> {
                let result = Set::default();
                while let Some(key) = entries.next_element()? {
                    result.insert(key);
                }
                Ok(result)
            }
        }
        d.deserialize_seq(SetVisitor(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use rstest::rstest;

    use super::*;
    use crate::target_graph::MinimizedBuckTarget;
    use crate::target_graph::RawId;
    use crate::target_graph::TargetGraph;
    use crate::target_graph::TargetId;
    use crate::types::TargetHash;

    #[test]
    fn json_visitors_preserve_duplicate_and_error_contracts() -> anyhow::Result<()> {
        let map: Map<TargetId, Vec<TargetId>> = serde_json::from_str(r#"{"1":[1],"1":[2]}"#)?;
        assert_eq!(map.len(), 1);
        assert_eq!(
            map.get(&TargetId::from_raw(1)).unwrap().to_owned_value(),
            vec![TargetId::from_raw(2)]
        );
        let set: Set<TargetId> = serde_json::from_str("[1,1,2]")?;
        assert_eq!(set.len(), 2);
        assert!(set.contains(&TargetId::from_raw(1)));
        assert!(set.contains(&TargetId::from_raw(2)));
        assert!(
            serde_json::from_str::<Map<TargetId, Vec<TargetId>>>(r#"{"1":"invalid"}"#).is_err()
        );
        assert!(serde_json::from_str::<Set<TargetId>>("[null]").is_err());
        Ok(())
    }

    fn map_with_one_row(loaded: bool) -> anyhow::Result<Map<TargetId, Vec<TargetId>>> {
        let key = TargetId::from_raw(1);
        if loaded {
            let mut chunk = super::super::Chunk::new(1);
            chunk.push(key.raw(), &vec![key])?;
            Map::from_chunks(vec![chunk], &|| Ok(()))
        } else {
            let map = Map::default();
            map.insert(key, vec![key]);
            Ok(map)
        }
    }

    #[rstest]
    #[case(false)]
    #[case(true)]
    fn vacant_entries_and_reinsertions_preserve_logical_length(
        #[case] loaded: bool,
    ) -> anyhow::Result<()> {
        let map = map_with_one_row(loaded)?;
        let old = TargetId::from_raw(1);
        let new = TargetId::from_raw(2);
        assert!(map.entry(new).get().is_none());
        assert_eq!(map.len(), 1);
        assert_eq!(serde_json::to_value(&map)?.as_object().unwrap().len(), 1);
        assert!(map.get_mut(&new).is_none());
        map.entry(new).or_insert(vec![old]);
        assert_eq!(map.len(), 2);
        assert!(!map.insert(new, vec![new]));
        assert_eq!(map.len(), 2);
        assert!(map.discard(&old));
        assert!(map.entry(old).get().is_none());
        assert_eq!(map.len(), 1);
        assert!(!map.discard(&old));
        map.entry(old).or_default().push(new);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&old).unwrap().to_owned_value(), vec![new]);
        assert_eq!(map.iter().count(), 2);
        Ok(())
    }

    #[test]
    fn inserting_base_rows_preserves_membership_and_length() -> anyhow::Result<()> {
        let loaded = true;
        let map = map_with_one_row(loaded)?;
        let key = TargetId::from_raw(1);
        let value = TargetId::from_raw(2);

        assert!(!map.insert(key, vec![value]));
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&key).unwrap().to_owned_value(), vec![value]);
        assert!(!map.insert(key, vec![key, value]));
        assert_eq!(map.len(), 1);

        assert!(map.discard(&key));
        assert_eq!(map.len(), 0);
        assert!(map.insert(key, vec![value, key]));
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&key).unwrap().to_owned_value(), vec![value, key]);
        assert_eq!(map.iter().count(), 1);
        Ok(())
    }

    #[rstest]
    #[case(false)]
    #[case(true)]
    fn concurrent_new_rows_are_counted_once(#[case] loaded: bool) -> anyhow::Result<()> {
        let map = map_with_one_row(loaded)?;
        let start = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let (map, start) = (&map, &start);
                scope.spawn(move || {
                    start.wait();
                    for id in 10..74 {
                        let key = TargetId::from_raw(id);
                        map.entry(key).or_insert(vec![key]);
                    }
                });
            }
        });
        assert_eq!(map.len(), 65);
        assert_eq!(map.iter().count(), 65);
        for id in 10..74 {
            let key = TargetId::from_raw(id);
            assert_eq!(map.remove(&key), Some((key, vec![key])));
        }
        assert_eq!(map.len(), 1);
        assert_eq!(map.par_iter().count(), 1);
        Ok(())
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct MustStayBorrowed;
    impl Value for MustStayBorrowed {
        fn encode(&self, _: &mut dyn std::io::Write) -> std::io::Result<()> {
            Ok(())
        }
        fn validate(bytes: &[u8]) -> anyhow::Result<()> {
            anyhow::ensure!(bytes.is_empty(), "payload");
            Ok(())
        }
        fn decode(_: &[u8]) -> Self {
            panic!("discard must not materialize a native value")
        }
    }

    #[test]
    fn discard_does_not_decode_and_remove_preserves_owned_values() -> anyhow::Result<()> {
        let key = TargetId::from_raw(1);
        let mut chunk = super::super::Chunk::new(1);
        chunk.push(key.raw(), &MustStayBorrowed)?;
        let map = Map::<TargetId, MustStayBorrowed>::from_chunks(vec![chunk], &|| Ok(()))?;
        assert!(map.discard(&key));
        assert!(!map.discard(&key));
        assert_eq!(map.len(), 0);
        map.insert(key, MustStayBorrowed);
        assert_eq!(map.remove(&key), Some((key, MustStayBorrowed)));
        assert_eq!(map.iter().count(), 0);
        Ok(())
    }

    #[test]
    fn publication_before_insertion_keeps_the_base_visible() -> anyhow::Result<()> {
        let key = TargetId::from_raw(1);
        let mut chunk = super::super::Chunk::new(1);
        chunk.push(key.raw(), &vec![key])?;
        let map = Map::<TargetId, Vec<TargetId>>::from_chunks(vec![chunk], &|| Ok(()))?;
        std::thread::scope(|scope| {
            let (published, ready) = mpsc::channel();
            let (proceed, resume) = mpsc::channel();
            let map_ref = &map;
            let writer = scope.spawn(move || {
                map_ref.mark_override(map_ref.base_row(key).unwrap());
                published.send(()).unwrap();
                resume.recv().unwrap();
                map_ref.insert(key, vec![]);
            });
            ready.recv().unwrap();
            let value = map.get(&key).map(|r| r.to_owned_value());
            let entries = map.iter().count();
            proceed.send(()).unwrap();
            writer.join().unwrap();
            assert_eq!(value, Some(vec![key]));
            assert_eq!(entries, 1);
            assert_eq!(map.get(&key).unwrap().to_owned_value(), vec![]);
        });
        Ok(())
    }

    #[test]
    fn concurrent_updates_preserve_every_append() -> anyhow::Result<()> {
        let key = TargetId::from_raw(1);
        let mut chunk = super::super::Chunk::new(1);
        chunk.push(key.raw(), &Vec::<TargetId>::new())?;
        let map = Map::<TargetId, Vec<TargetId>>::from_chunks(vec![chunk], &|| Ok(()))?;
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let map = &map;
                scope.spawn(move || {
                    for _ in 0..64 {
                        map.entry(key).or_default().push(key);
                    }
                });
            }
        });
        assert_eq!(map.get(&key).unwrap().to_owned_value(), vec![key; 512]);
        assert_eq!(map.len(), 1);
        assert_eq!(map.par_iter().count(), 1);
        Ok(())
    }

    #[test]
    fn interning_loaded_labels_does_not_create_an_override() -> anyhow::Result<()> {
        let graph = TargetGraph::new();
        let label = graph.store_label("ci:test");
        let id = graph.store_label_set(vec![label]);
        let mut file = tempfile::tempfile()?;
        graph.write_framed(&mut file)?;
        let loaded = TargetGraph::read_framed(&file)?;
        assert_eq!(loaded.store_label_set(vec![label]), id);
        assert!(loaded.label_set_id_to_labels.changes.is_empty());
        Ok(())
    }

    #[test]
    fn unrelated_base_reads_do_not_wait_for_a_changed_row() -> anyhow::Result<()> {
        let a = TargetId::from_raw(1);
        let b = TargetId::from_raw(2);
        let mut chunk = super::super::Chunk::new(2);
        chunk.push(a.raw(), &vec![a])?;
        chunk.push(b.raw(), &vec![b])?;
        let map = Map::<TargetId, Vec<TargetId>>::from_chunks(vec![chunk], &|| Ok(()))?;
        let mut held = map.entry(a).or_default();
        held.push(b);
        assert!(matches!(
            map.changes.try_get(&b.raw()),
            dashmap::try_result::TryResult::Locked
        ));
        std::thread::scope(|scope| {
            let (send, receive) = mpsc::channel();
            let map_ref = &map;
            let reader = scope.spawn(move || {
                send.send(map_ref.get(&b).unwrap().to_owned_value())
                    .unwrap();
            });
            let observed = receive.recv_timeout(Duration::from_secs(5));
            drop(held);
            reader.join().unwrap();
            assert_eq!(
                observed.expect("unchanged base read must not take the change-map lock"),
                vec![b]
            );
        });
        Ok(())
    }

    #[test]
    fn misfiled_cleanup_does_not_copy_unrelated_packages() -> anyhow::Result<()> {
        let graph = TargetGraph::new();
        let a = graph.store_target("fbcode//a:a");
        let a2 = graph.store_target("fbcode//a:other");
        let b = graph.store_target("fbcode//b:b");
        let c = graph.store_target("fbcode//c:c");
        let pa = graph.store_package("fbcode//a");
        let pb = graph.store_package("fbcode//b");
        let pc = graph.store_package("fbcode//c");
        let rule_type = graph.store_rule_type("rust_library");
        for target in [a, a2, b, c] {
            graph.store_minimized_target(
                target,
                MinimizedBuckTarget {
                    rule_type,
                    oncall: None,
                    labels: vec![],
                    target_hash: TargetHash::new("1"),
                },
            );
        }
        graph.add_target_to_package(pa, a2);
        graph.add_target_to_package(pb, a);
        graph.add_target_to_package(pb, b);
        graph.add_target_to_package(pc, c);
        let mut file = tempfile::tempfile()?;
        graph.write_framed(&mut file)?;
        let loaded = TargetGraph::read_framed(&file)?;
        loaded.remove_targets_batch(&[a].into_iter().collect());
        assert_eq!(
            loaded
                .package_id_to_targets
                .get(&pb)
                .unwrap()
                .to_owned_value(),
            vec![b]
        );
        assert!(!loaded.package_id_to_targets.changes.contains_key(&pa.raw()));
        assert!(!loaded.package_id_to_targets.changes.contains_key(&pc.raw()));
        Ok(())
    }
}
