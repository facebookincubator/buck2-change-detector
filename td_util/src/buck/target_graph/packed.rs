/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Runtime field storage, independent of the persistent graph codec.
//!
//! A field keeps decoded blocks and stable hash keys. Edge payloads can retain
//! dense indices with a shared ID dictionary. The dictionary never changes;
//! mutable replacement rows contain stable IDs instead. Value validation occurs
//! before the field is exposed, so borrowed access can rely on these invariants.
//! The on-disk codec must still validate its own representation before building
//! these blocks. Runtime bytes are not a second serialized graph format.
mod field;
mod value;

use std::sync::Arc;

pub(super) use field::Map;
pub(super) use field::ReadRef;
pub(super) use field::Set;
use rayon::prelude::*;
pub(super) use value::Value;

use super::CheckCancelled;
pub(super) use super::RawId as Key;

const ROW_BATCH: usize = 1 << 16;
const MAX_INDEX_PARTITIONS: usize = 256;
const MAX_INDEX_PROBES: usize = 128;
const EMPTY_SLOT: u32 = u32::MAX;
pub(super) const WORD_BYTES: usize = size_of::<u64>();
pub(super) const INDEX_BYTES: usize = size_of::<u32>();

fn word(bytes: &[u8], pos: usize) -> anyhow::Result<u64> {
    use anyhow::Context;
    Ok(u64::from_le_bytes(
        bytes
            .get(pos..pos + WORD_BYTES)
            .context("short packed word")?
            .try_into()?,
    ))
}

#[derive(Debug, Default)]
pub(super) struct Chunk {
    pub(super) keys: Vec<u64>,
    pub(super) offsets: Vec<usize>,
    pub(super) data: Vec<u8>,
}
impl Chunk {
    pub(super) fn new(entries: usize) -> Self {
        let mut offsets = Vec::with_capacity(entries + 1);
        offsets.push(0);
        Self {
            keys: Vec::with_capacity(entries),
            offsets,
            data: Vec::new(),
        }
    }
    pub(super) fn finish_row(&mut self, key: u64) {
        self.keys.push(key);
        self.offsets.push(self.data.len());
    }
    pub(super) fn push<V: Value>(&mut self, key: u64, value: &V) -> anyhow::Result<()> {
        value.encode(&mut self.data)?;
        self.finish_row(key);
        Ok(())
    }
}

#[derive(Debug)]
struct Row {
    key: u64,
    chunk: u32,
    local: u32,
}

#[derive(Debug)]
struct Column {
    chunks: Vec<Chunk>,
    rows: Vec<Row>,
    indexes: Vec<Index>,
    shift: u32,
    ids: Option<Arc<Vec<u64>>>,
}
#[derive(Clone, Copy)]
pub(super) struct View<'a> {
    bytes: &'a [u8],
    ids: Option<&'a [u64]>,
}
impl Column {
    fn from_chunks(mut chunks: Vec<Chunk>, check: &CheckCancelled<'_>) -> anyhow::Result<Self> {
        check()?;
        let count: usize = chunks.iter().map(|c| c.keys.len()).sum();
        anyhow::ensure!(
            count < u32::MAX as usize && chunks.len() < u32::MAX as usize,
            "packed index capacity"
        );
        let mut rows = Vec::with_capacity(count);
        for (i, chunk) in chunks.iter_mut().enumerate() {
            check()?;
            anyhow::ensure!(
                chunk.offsets.len() == chunk.keys.len() + 1,
                "packed row count"
            );
            for (j, key) in std::mem::take(&mut chunk.keys).into_iter().enumerate() {
                rows.push(Row {
                    key,
                    chunk: i as u32,
                    local: j as u32,
                });
            }
        }
        let groups = (count / ROW_BATCH)
            .max(1)
            .next_power_of_two()
            .min(MAX_INDEX_PARTITIONS);
        let shift = u64::BITS - groups.trailing_zeros();
        let mut pairs = (0..groups)
            .map(|_| Vec::with_capacity(count / groups + 1))
            .collect::<Vec<_>>();
        for (part, items) in rows.chunks(ROW_BATCH).enumerate() {
            check()?;
            for (offset, row) in items.iter().enumerate() {
                let group = if groups == 1 {
                    0
                } else {
                    (row.key >> shift) as usize
                };
                pairs[group].push((row.key, (part * ROW_BATCH + offset) as u32));
            }
        }
        let indexes = pairs
            .into_par_iter()
            .map(|pairs| Index::new(pairs, check))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            chunks,
            rows,
            indexes,
            shift,
            ids: None,
        })
    }

    fn len(&self) -> usize {
        self.rows.len()
    }
    fn key(&self, row: usize) -> u64 {
        self.rows[row].key
    }
    fn value(&self, row: usize) -> View<'_> {
        let row = &self.rows[row];
        let chunk = &self.chunks[row.chunk as usize];
        let i = row.local as usize;
        View {
            bytes: &chunk.data[chunk.offsets[i]..chunk.offsets[i + 1]],
            ids: self.ids.as_deref().map(Vec::as_slice),
        }
    }
    fn find(&self, key: u64) -> Option<usize> {
        let group = if self.indexes.len() == 1 {
            0
        } else {
            (key >> self.shift) as usize
        };
        let index = &self.indexes[group];
        if !index.fallback.is_empty() {
            let i = index.fallback.binary_search_by_key(&key, |r| r.0).ok()?;
            return Some(index.fallback[i].1 as usize);
        }
        let home = key as usize & (index.slots.len() - 1);
        for probe in 0..MAX_INDEX_PROBES {
            let row = index.slots[(home + probe) & (index.slots.len() - 1)];
            if row == EMPTY_SLOT {
                return None;
            }
            if self.key(row as usize - 1) == key {
                return Some(row as usize - 1);
            }
        }
        None
    }
    fn validate_values<V: Value>(&self, check: &CheckCancelled<'_>) -> anyhow::Result<()> {
        self.rows
            .par_chunks(ROW_BATCH)
            .enumerate()
            .try_for_each(|(part, rows)| {
                check()?;
                for offset in 0..rows.len() {
                    V::validate_view(self.value(part * ROW_BATCH + offset))?;
                }
                Ok(())
            })
    }
}

#[derive(Debug)]
struct Index {
    slots: Vec<u32>,
    fallback: Vec<(u64, u32)>,
}
impl Index {
    fn new(mut pairs: Vec<(u64, u32)>, check: &CheckCancelled<'_>) -> anyhow::Result<Self> {
        check()?;
        // Nonzero initialization gives probes private, writable pages. Reading
        // a calloc zero page before the first insertion would trigger a
        // copy-on-write fault and cross-CPU TLB invalidation on that write.
        let mut slots = vec![EMPTY_SLOT; (pairs.len() * 2).max(1).next_power_of_two()];
        'rows: for (row, &(key, _)) in pairs.iter().enumerate() {
            if row.is_multiple_of(ROW_BATCH) {
                check()?;
            }
            let home = key as usize & (slots.len() - 1);
            for probe in 0..MAX_INDEX_PROBES {
                let pos = (home + probe) & (slots.len() - 1);
                let old = slots[pos];
                if old == EMPTY_SLOT {
                    slots[pos] = row as u32 + 1;
                    continue 'rows;
                }
                anyhow::ensure!(pairs[old as usize - 1].0 != key, "duplicate packed key");
            }
            pairs.sort_unstable_by_key(|r| r.0);
            check()?;
            anyhow::ensure!(
                pairs.windows(2).all(|w| w[0].0 != w[1].0),
                "duplicate packed key"
            );
            return Ok(Self {
                slots: Vec::new(),
                fallback: pairs,
            });
        }
        check()?;
        for slot in &mut slots {
            if *slot != EMPTY_SLOT {
                *slot = pairs[*slot as usize - 1].1 + 1;
            }
        }
        Ok(Self {
            slots,
            fallback: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::RawId;
    use super::super::TargetId;
    use super::*;

    #[test]
    fn duplicate_keys_are_rejected_in_probe_and_sorted_indexes() {
        let direct = vec![(1, 0), (1, 1)];
        let mut clustered: Vec<_> = (0..=MAX_INDEX_PROBES)
            .map(|i| ((i as u64) << 16, i as u32))
            .collect();
        clustered.push((0, clustered.len() as u32));
        for pairs in [direct, clustered] {
            let error = Index::new(pairs, &|| Ok(())).unwrap_err();
            assert!(error.to_string().contains("duplicate packed key"));
        }
    }

    #[test]
    fn runtime_assembly_propagates_cancellation() -> anyhow::Result<()> {
        let chunk = || {
            let mut chunk = Chunk::new(1);
            chunk.push(1, &String::from("value")).unwrap();
            chunk
        };
        let cancelled = || anyhow::bail!("cancelled runtime construction");
        assert!(Column::from_chunks(vec![chunk()], &cancelled).is_err());
        assert!(Index::new(vec![(1, 0)], &cancelled).is_err());
        let column = Column::from_chunks(vec![chunk()], &|| Ok(()))?;
        assert!(column.validate_values::<String>(&cancelled).is_err());
        Ok(())
    }
    #[test]
    fn base_updates_deletions_and_iteration_form_one_map() -> anyhow::Result<()> {
        let a = TargetId::from_raw(1);
        let b = TargetId::from_raw(2);
        let c = TargetId::from_raw(3);
        let mut chunk = Chunk::new(2);
        chunk.push(a.raw(), &vec![a, b])?;
        chunk.push(b.raw(), &vec![a])?;
        let map = Map::<TargetId, Vec<TargetId>>::from_chunks(vec![chunk], &|| Ok(()))?;
        map.entry(a).or_default().push(c);
        map.remove(&b);
        map.insert(c, vec![b]);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&a).unwrap().to_owned_value(), vec![a, b, c]);
        assert!(map.get(&b).is_none());
        assert_eq!(map.iter().count(), 2);
        assert_eq!(map.par_iter().count(), 2);
        Ok(())
    }
    #[test]
    fn clustered_keys_have_a_bounded_index_fallback() -> anyhow::Result<()> {
        let mut chunk = Chunk::new(1024);
        for key in 0..1024 {
            chunk.push(key * 2048, &String::from("value"))?;
        }
        let map = Map::<TargetId, String>::from_chunks(vec![chunk], &|| Ok(()))?;
        for key in 0..1024 {
            assert_eq!(
                map.get(&TargetId::from_raw(key * 2048)).unwrap().text(),
                "value"
            );
        }
        assert!(map.get(&TargetId::from_raw(1)).is_none());
        Ok(())
    }
    #[test]
    fn dictionary_positions_do_not_escape_into_mutable_ids() -> anyhow::Result<()> {
        let mut chunk = Chunk::new(1);
        chunk.data.extend_from_slice(&1u32.to_le_bytes());
        chunk.data.extend_from_slice(&0u32.to_le_bytes());
        chunk.finish_row(1);
        let map = Map::<TargetId, Vec<TargetId>>::from_list_chunks(
            vec![chunk],
            Arc::new(vec![100, 200]),
            &|| Ok(()),
        )?;
        let key = TargetId::from_raw(1);
        let a = TargetId::from_raw(100);
        let b = TargetId::from_raw(200);
        let c = TargetId::from_raw(300);
        let mut out = Vec::new();
        map.get(&key).unwrap().append_to(&mut out);
        assert_eq!(out, vec![b, a]);
        assert_eq!(map.get(&key).unwrap().ids_len(), 2);
        out.clear();
        map.get(&key).unwrap().for_each_id(|id| out.push(id));
        assert_eq!(out, vec![b, a]);
        map.entry(key).or_default().push(c);
        assert_eq!(map.get(&key).unwrap().to_owned_value(), vec![b, a, c]);
        assert_eq!(map.get(&key).unwrap().ids_len(), 3);
        out.clear();
        map.get(&key).unwrap().for_each_id(|id| out.push(id));
        assert_eq!(out, vec![b, a, c]);
        map.remove(&key);
        map.insert(key, vec![c, b]);
        assert_eq!(map.get(&key).unwrap().to_owned_value(), vec![c, b]);
        Ok(())
    }

    #[test]
    fn index_routes_global_rows_across_chunks_and_prefixes() -> anyhow::Result<()> {
        let mut chunks = Vec::new();
        for part in 0..4u64 {
            let mut chunk = Chunk::new(40_000);
            for i in part * 40_000..(part + 1) * 40_000 {
                let key = i.wrapping_mul(0x9e3779b97f4a7c15);
                chunk.push(key, &vec![TargetId::from_raw(i)])?;
            }
            chunks.push(chunk);
        }
        let map = Map::<TargetId, Vec<TargetId>>::from_chunks(chunks, &|| Ok(()))?;
        for i in 0..160_000u64 {
            let key = TargetId::from_raw(i.wrapping_mul(0x9e3779b97f4a7c15));
            assert_eq!(
                map.get(&key).unwrap().to_owned_value(),
                vec![TargetId::from_raw(i)]
            );
        }
        assert_eq!(map.iter().count(), 160_000);
        Ok(())
    }
}
