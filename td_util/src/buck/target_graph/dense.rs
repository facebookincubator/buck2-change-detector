/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The binary layout of `TargetGraph` for graph schema `SCHEMA_VERSION`.
//!
//! The in-memory graph keys every field by a content hash. Random 64-bit keys
//! do not compress, so on disk each id space is numbered instead by the sorted
//! order of its strings. String tables carry no keys: the reader hashes each
//! string to recover its id. Maps are stored as columns in key order, and
//! unordered edge lists are sorted, so neighbouring entries share prefixes and
//! small gaps.
//!
//! ```text
//! file := MAGIC schema:u32 part* directory directory_offset:u64 MAGIC
//! ```
//!
//! Every field is a stream of blocks. A block has an index part, whose first
//! region holds one varint per entry, and a data part. Each part is an
//! independent zstd frame, so blocks decode in parallel. The directory is a
//! zstd frame too. Every frame carries a checksum of its content, so the
//! reader rejects a change that alters any decoded byte. The reader
//! decodes a stream's index parts first and sizes its allocations from them,
//! not from the counts that the directory claims.

use std::fs::File;
use std::io::Write;
use std::ops::Range;

use anyhow::Context;
use dashmap::mapref::multiple::RefMulti;
use serde::Deserialize;
use serde::Serialize;

use super::*;

/// Every layout of the graph file starts with this magic and the schema
/// number, so a reader of any schema reports a version mismatch, not a
/// corrupt file.
const MAGIC: [u8; 4] = *b"TGRF";
/// Magic and schema number.
const HEADER_BYTES: u64 = (MAGIC.len() + size_of::<u32>()) as u64;
/// Directory offset and the trailing magic.
const TRAILER_BYTES: u64 = (size_of::<u64>() + MAGIC.len()) as u64;
/// Byte 4 of a zstd frame is its header descriptor; this bit marks a content
/// checksum.
const FRAME_DESCRIPTOR_BYTE: usize = 4;
const FRAME_CHECKSUM_FLAG: u8 = 0b100;
/// Largest zstd window the reader accepts. One huge edge list can make a large
/// part with a large window.
const MAX_WINDOW_LOG: u32 = 31;
const ZSTD_LEVEL: i32 = 9;
/// Largest uncompressed index or data part. Only a single huge edge list gets
/// near it.
const MAX_PART_BYTES: u64 = 1 << 30;
const MAX_DIRECTORY_BYTES: usize = 64 << 20;
/// Edge lists longer than this are mapped and sorted in parallel.
const HUB_LIST: usize = 1 << 16;

/// Block sizes. Smaller blocks decode with more parallelism; larger blocks
/// compress better.
#[derive(Clone, Copy, Debug)]
struct Layout {
    /// Entries per block of a map.
    map_entries: usize,
    /// Strings per block of a string table.
    strings: usize,
    /// Upper bound on the list values, or message bytes, in one block.
    values: usize,
}

const LAYOUT: Layout = Layout {
    map_entries: 1 << 16,
    strings: 1 << 16,
    values: 1 << 20,
};

/// How the values of an edge list are encoded.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
enum ListMode {
    /// In stored order, as varints. For lists whose order is meaningful.
    Raw,
    /// Sorted; the first value absolute, then gaps.
    Gaps,
    /// Sorted; the first value relative to the key (zigzag), then gaps.
    GapsRel,
    /// Sorted; fixed-width little-endian u32 indices, so the compressor can
    /// match repeated lists.
    Abs32,
}

/// Every edge-list field: (stream name, field, key table, value table, mode).
/// Registering ids, writing and reading all expand this one table.
macro_rules! list_streams {
    ($callback:ident) => {
        $callback! {
            ("deps", target_id_to_deps, target, target, Abs32),
            ("rdeps", target_id_to_rdeps, target, target, GapsRel),
            ("pkg_targets", package_id_to_targets, package, target, Gaps),
            ("file_deps", file_id_to_deps, file, file, Gaps),
            ("file_rdeps", file_id_to_rdeps, file, file, Gaps),
            ("ci_srcs", target_id_to_ci_srcs, target, glob, Raw),
            ("ci_srcs_mm", target_id_to_ci_srcs_must_match, target, glob, Raw),
            ("cidep_pkg", target_id_to_ci_deps_package_patterns, target, cidep, Raw),
            ("cidep_rec", target_id_to_ci_deps_recursive_patterns, target, cidep, Raw),
            ("hint_to_affected", ci_hint_to_affected, target, target, Gaps),
            ("affected_to_hints", affected_to_ci_hints, target, target, Gaps),
        }
    };
}

// ---------- directory ----------

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
struct Part {
    offset: u64,
    comp: u64,
    uncomp: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Block {
    entries: u64,
    index: Part,
    data: Part,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Stream {
    name: String,
    /// How an edge list encodes its values; other streams have none. The
    /// reader decodes each list with its stored mode, so `list_streams!` can
    /// change a mode without a schema bump.
    mode: Option<ListMode>,
    blocks: Vec<Block>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Directory {
    streams: Vec<Stream>,
}

impl Directory {
    fn stream(&self, name: &str) -> anyhow::Result<&Stream> {
        self.streams
            .iter()
            .find(|s| s.name == name)
            .with_context(|| format!("missing stream {name}"))
    }
}

// ---------- varints and regions ----------

#[inline]
fn put(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

#[inline]
fn zigzag(x: i64) -> u64 {
    ((x << 1) ^ (x >> 63)) as u64
}

#[inline]
fn unzigzag(x: u64) -> i64 {
    ((x >> 1) as i64) ^ -((x & 1) as i64)
}

struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }

    /// A LEB128 varint of at most 64 bits.
    #[inline]
    fn get(&mut self) -> anyhow::Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let Some(&byte) = self.b.get(self.i) else {
                return Err(truncated());
            };
            self.i += 1;
            // The tenth byte holds only bit 63.
            anyhow::ensure!(shift < 63 || byte <= 1, "varint exceeds 64 bits");
            v |= ((byte & 0x7f) as u64) << shift;
            if byte < 0x80 {
                return Ok(v);
            }
        }
        unreachable!("the tenth byte always ends the varint or fails")
    }

    /// A varint used as a length or position.
    #[inline]
    fn index(&mut self) -> anyhow::Result<usize> {
        Ok(usize::try_from(self.get()?)?)
    }

    /// A count of items that each take at least one more byte of this region,
    /// so the count bounds the allocation it sizes.
    #[inline]
    fn count(&mut self, rest: &Cursor<'_>) -> anyhow::Result<usize> {
        let n = self.index()?;
        anyhow::ensure!(
            n <= rest.remaining(),
            "{n} items in {} remaining bytes",
            rest.remaining()
        );
        Ok(n)
    }

    fn bytes(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        let end = self
            .i
            .checked_add(n)
            .filter(|&end| end <= self.b.len())
            .ok_or_else(truncated)?;
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> anyhow::Result<[u8; N]> {
        Ok(self
            .bytes(N)?
            .try_into()
            .expect("bytes returns exactly N bytes"))
    }

    fn remaining(&self) -> usize {
        self.b.len() - self.i
    }

    fn done(&self) -> bool {
        self.i == self.b.len()
    }
}

#[cold]
fn truncated() -> anyhow::Error {
    anyhow::anyhow!("truncated block")
}

/// Strictly increasing key indices of one block, stored as the first index
/// plus one, then the gaps.
#[derive(Default)]
struct Keys {
    prev: Option<usize>,
}

impl Keys {
    #[inline]
    fn next(&mut self, c: &mut Cursor<'_>) -> anyhow::Result<usize> {
        let d = c.index()?;
        let key = match self.prev {
            None => d.checked_sub(1),
            Some(prev) => prev.checked_add(d).filter(|_| d > 0),
        }
        .context("key indices are not strictly increasing")?;
        self.prev = Some(key);
        Ok(key)
    }
}

/// The writing side of `Keys`.
#[derive(Default)]
struct KeyWriter {
    prev: Option<u32>,
}

impl KeyWriter {
    #[inline]
    fn put(&mut self, buf: &mut Vec<u8>, key: u32) {
        let gap = match self.prev {
            None => key as u64 + 1,
            Some(prev) => (key - prev) as u64,
        };
        put(buf, gap);
        self.prev = Some(key);
    }
}

/// The id at a file-supplied index.
#[inline]
fn at(ids: &[u64], i: u64) -> anyhow::Result<u64> {
    usize::try_from(i)
        .ok()
        .and_then(|i| ids.get(i))
        .copied()
        .ok_or_else(|| out_of_range(i, ids.len()))
}

#[cold]
fn out_of_range(i: u64, len: usize) -> anyhow::Error {
    anyhow::anyhow!("index {i} out of range for {len} ids")
}

/// A block part: the region count and lengths, then the regions back to back.
fn pack_regions(regions: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(regions.iter().map(|r| r.len() + 10).sum::<usize>() + 4);
    put(&mut out, regions.len() as u64);
    for r in regions {
        put(&mut out, r.len() as u64);
    }
    for r in regions {
        out.extend_from_slice(r);
    }
    out
}

/// Splits a block part into exactly `n` regions, each with its own cursor.
fn cursors(part: &[u8], n: usize) -> anyhow::Result<Vec<Cursor<'_>>> {
    let mut c = Cursor::new(part);
    let found = c.index()?;
    anyhow::ensure!(found == n, "block has {found} regions, expected {n}");
    let lens = (0..n)
        .map(|_| c.index())
        .collect::<anyhow::Result<Vec<_>>>()?;
    let regions = lens
        .into_iter()
        .map(|len| Ok(Cursor::new(c.bytes(len)?)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(c.done(), "trailing bytes after the regions of a block");
    Ok(regions)
}

/// Every region of every part was read to its end.
fn all_done(parts: &[&[Cursor<'_>]]) -> bool {
    parts.iter().all(|cursors| cursors.iter().all(Cursor::done))
}

/// The id of a string. Ids are hashes of their strings.
fn id_of<K: RawId>(s: &str) -> K {
    match s.parse() {
        Ok(id) => id,
        Err(never) => match never {},
    }
}

// ---------- writer: id tables ----------

/// Sorted (id, index) pairs with a top-bits bucket directory, so a lookup is
/// one bucket read plus a scan of about one entry.
struct IdIndex {
    pairs: Vec<(u64, u32)>,
    buckets: Vec<u32>,
    shift: u32,
}

impl IdIndex {
    fn new(ids: impl IndexedParallelIterator<Item = u64>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            ids.len() <= u32::MAX as usize,
            "{} ids are more than a u32 index can number",
            ids.len()
        );
        let mut pairs: Vec<(u64, u32)> = ids.enumerate().map(|(i, id)| (id, i as u32)).collect();
        pairs.par_sort_unstable_by_key(|p| p.0);
        let bits = (usize::BITS - pairs.len().max(2).leading_zeros()).min(30);
        let shift = 64 - bits;
        let nb = 1usize << bits;
        let mut buckets = vec![0u32; nb + 1];
        let mut j = 0usize;
        for (b, slot) in buckets.iter_mut().enumerate().take(nb) {
            while j < pairs.len() && ((pairs[j].0 >> shift) as usize) < b {
                j += 1;
            }
            *slot = j as u32;
        }
        buckets[nb] = pairs.len() as u32;
        Ok(Self {
            pairs,
            buckets,
            shift,
        })
    }

    #[inline]
    fn get(&self, id: u64) -> Option<u32> {
        let b = (id >> self.shift) as usize;
        let (lo, hi) = (self.buckets[b] as usize, self.buckets[b + 1] as usize);
        self.pairs[lo..hi].iter().find(|p| p.0 == id).map(|p| p.1)
    }
}

/// One id space as numbered on disk: its strings in sorted order, then the
/// ids that some field references but no string defines ("extras"), in id
/// order. Holds read guards on the graph, so strings are never copied.
struct Table<'g, K> {
    name: &'static str,
    refs: Vec<RefMulti<'g, K, String>>,
    extras: Vec<u64>,
    /// The on-disk index of each string's id.
    index: IdIndex,
    /// The position of each extra in `extras`.
    extras_index: IdIndex,
}

impl<'g, K: RawId> Table<'g, K> {
    fn build(
        name: &'static str,
        field: &'g ShardedField<IdDashMap<K, String>>,
    ) -> anyhow::Result<Self> {
        let mut refs: Vec<RefMulti<'g, K, String>> = field.par_iter().collect();
        refs.par_sort_unstable_by(|a, b| a.value().cmp(b.value()));
        let bad = refs
            .par_iter()
            .filter(|r| id_of::<K>(r.value()) != *r.key())
            .count();
        anyhow::ensure!(
            bad == 0,
            "table {name}: {bad} keys are not the hash of their string"
        );
        let index = IdIndex::new(refs.par_iter().map(|r| r.key().raw()))?;
        Ok(Self {
            name,
            refs,
            extras: Vec::new(),
            index,
            extras_index: IdIndex::new(rayon::iter::empty())?,
        })
    }

    fn contains(&self, id: u64) -> bool {
        self.index.get(id).is_some()
    }

    fn len(&self) -> usize {
        self.refs.len() + self.extras.len()
    }

    fn id_at(&self, i: usize) -> u64 {
        match self.refs.get(i) {
            Some(r) => r.key().raw(),
            None => self.extras[i - self.refs.len()],
        }
    }

    /// The ids in `ids` that this table has no string for.
    fn missing(&self, ids: impl ParallelIterator<Item = u64>) -> Vec<u64> {
        ids.filter(|&id| !self.contains(id)).collect()
    }

    /// The list values of `field` that this table has no string for.
    fn missing_values<J: RawId>(&self, field: &ShardedField<IdDashMap<J, Vec<K>>>) -> Vec<u64> {
        field
            .par_iter()
            .flat_map_iter(|e| {
                e.value()
                    .iter()
                    .map(|v| v.raw())
                    .filter(|&id| !self.contains(id))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Numbers the ids without a string after every string, in id order, so
    /// one graph always encodes to the same bytes.
    fn set_extras(&mut self, mut ids: Vec<u64>) -> anyhow::Result<()> {
        ids.par_sort_unstable();
        ids.dedup();
        anyhow::ensure!(
            self.refs.len() + ids.len() <= u32::MAX as usize,
            "table {} has more ids than a u32 index can number",
            self.name
        );
        self.extras_index = IdIndex::new(ids.par_iter().copied())?;
        self.extras = ids;
        Ok(())
    }

    /// The on-disk index of `id`. `Tables::build` registers every id that a
    /// stream can reference as a string or an extra, so a miss is a bug there.
    #[inline]
    fn index_of(&self, id: u64) -> u32 {
        self.index
            .get(id)
            .or_else(|| Some(self.refs.len() as u32 + self.extras_index.get(id)?))
            .unwrap_or_else(|| panic!("table {}: id {id:x} was not registered", self.name))
    }
}

/// The id tables, by the names that `list_streams!` uses.
struct Tables<'g> {
    target: Table<'g, TargetId>,
    rule: Table<'g, RuleTypeId>,
    oncall: Table<'g, OncallId>,
    label: Table<'g, LabelId>,
    glob: Table<'g, GlobPatternId>,
    package: Table<'g, PackageId>,
    file: Table<'g, FileId>,
    cidep: Table<'g, CiDepsPatternId>,
}

/// One id vector per table, by the names that `list_streams!` uses. The
/// writer collects the ids without a string in it; the reader stores each
/// table's ids in on-disk order, so index i of a table is the id at position i.
#[derive(Default)]
struct Ids {
    target: Vec<u64>,
    rule: Vec<u64>,
    oncall: Vec<u64>,
    label: Vec<u64>,
    glob: Vec<u64>,
    package: Vec<u64>,
    file: Vec<u64>,
    cidep: Vec<u64>,
}

impl<'g> Tables<'g> {
    fn build(g: &'g TargetGraph) -> anyhow::Result<Self> {
        let (target, rest) = rayon::join(
            || Table::build("target", &g.target_id_to_label),
            || {
                Ok::<_, anyhow::Error>((
                    Table::build("rule", &g.rule_type_id_to_string)?,
                    Table::build("oncall", &g.oncall_id_to_string)?,
                    Table::build("label", &g.label_id_to_string)?,
                    Table::build("glob", &g.glob_pattern_id_to_string)?,
                    Table::build("package", &g.package_id_to_path)?,
                    Table::build("file", &g.file_id_to_path)?,
                    Table::build("cidep", &g.ci_deps_pattern_id_to_string)?,
                ))
            },
        );
        let (rule, oncall, label, glob, package, file, cidep) = rest?;
        let mut tables = Self {
            target: target?,
            rule,
            oncall,
            label,
            glob,
            package,
            file,
            cidep,
        };
        let missing = tables.missing(g);
        tables.set_extras(missing)?;
        Ok(tables)
    }

    /// Every id that some field references but no string defines. It is
    /// collected before any stream takes an index, so numbering is
    /// deterministic.
    fn missing(&self, g: &TargetGraph) -> Ids {
        let mut missing = Ids::default();
        macro_rules! lists {
            ($(($name:literal, $field:ident, $k:ident, $v:ident, $mode:ident)),* $(,)?) => {$(
                missing
                    .$k
                    .extend(self.$k.missing(g.$field.par_iter().map(|e| e.key().raw())));
                missing.$v.extend(self.$v.missing_values(&g.$field));
            )*};
        }
        list_streams!(lists);
        let minimized = || g.minimized_targets.par_iter();
        missing
            .target
            .extend(self.target.missing(minimized().map(|e| e.key().raw())));
        let sudo: Vec<u64> = g.targets_with_sudo_label.iter().map(|k| k.raw()).collect();
        missing
            .target
            .extend(self.target.missing(sudo.into_par_iter()));
        missing.rule.extend(
            self.rule
                .missing(minimized().map(|e| e.value().rule_type.raw())),
        );
        missing.oncall.extend(
            self.oncall
                .missing(minimized().filter_map(|e| e.value().oncall.map(RawId::raw))),
        );
        missing
            .label
            .extend(self.label.missing_values(&g.label_set_id_to_labels));
        missing.package.extend(
            self.package
                .missing(g.package_id_to_errors.par_iter().map(|e| e.key().raw())),
        );
        missing
    }

    fn set_extras(&mut self, m: Ids) -> anyhow::Result<()> {
        self.target.set_extras(m.target)?;
        self.rule.set_extras(m.rule)?;
        self.oncall.set_extras(m.oncall)?;
        self.label.set_extras(m.label)?;
        self.glob.set_extras(m.glob)?;
        self.package.set_extras(m.package)?;
        self.file.set_extras(m.file)?;
        self.cidep.set_extras(m.cidep)
    }
}

// ---------- writer: blocks ----------

/// One block before compression.
struct RawBlock {
    entries: u64,
    index: Vec<Vec<u8>>,
    data: Vec<Vec<u8>>,
}

/// Marks a key that the field does not hold.
const ABSENT: u32 = u32::MAX;

/// Cuts positions into ranges of at most `layout.map_entries` present entries
/// and, where possible, at most `layout.values` weight. Returns each range
/// with its entry count.
fn balanced_ranges(weights: &[u32], layout: Layout) -> Vec<(Range<usize>, u64)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut entries = 0u64;
    let mut acc = 0usize;
    for (i, &w) in weights.iter().enumerate() {
        if w == ABSENT {
            continue;
        }
        let w = w as usize;
        if entries > 0 && (entries as usize >= layout.map_entries || acc + w > layout.values) {
            out.push((start..i, entries));
            start = i;
            entries = 0;
            acc = 0;
        }
        entries += 1;
        acc += w;
    }
    if entries > 0 {
        out.push((start..weights.len(), entries));
    }
    out
}

/// How `Rows` walks a map in key order.
enum Order<'f, K, V> {
    /// By key-table index with point lookups, for a field that covers most of
    /// its table. Marks the indices that the field holds.
    ByTable(Vec<bool>),
    /// The field's entries, collected and sorted by key index.
    Sorted(Vec<(u32, RefMulti<'f, K, V>)>),
}

/// The entries of a map in on-disk key order, cut into blocks. Values are
/// never copied.
struct Rows<'f, 't, 'g, K, V> {
    field: &'f ShardedField<IdDashMap<K, V>>,
    keys: &'t Table<'g, K>,
    order: Order<'f, K, V>,
    blocks: Vec<(Range<usize>, u64)>,
}

impl<'f, 't, 'g, K, V> Rows<'f, 't, 'g, K, V>
where
    K: RawId,
    V: Send + Sync,
{
    fn new(
        field: &'f ShardedField<IdDashMap<K, V>>,
        keys: &'t Table<'g, K>,
        layout: Layout,
        weight: impl Fn(&V) -> usize + Sync,
    ) -> Self {
        let clamp = |w: usize| w.min(ABSENT as usize - 1) as u32;
        let n = keys.len();
        let (order, weights) = if field.len() * 2 > n {
            let weighted: Vec<(u32, u32)> = field
                .par_iter()
                .map(|e| (keys.index_of(e.key().raw()), clamp(weight(e.value()))))
                .collect();
            let mut weights = vec![ABSENT; n];
            for (i, w) in weighted {
                weights[i as usize] = w;
            }
            let present = weights.iter().map(|&w| w != ABSENT).collect();
            (Order::ByTable(present), weights)
        } else {
            let mut rows: Vec<(u32, RefMulti<'f, K, V>)> = field
                .par_iter()
                .map(|r| (keys.index_of(r.key().raw()), r))
                .collect();
            rows.par_sort_unstable_by_key(|r| r.0);
            let weights = rows.iter().map(|r| clamp(weight(r.1.value()))).collect();
            (Order::Sorted(rows), weights)
        };
        let blocks = balanced_ranges(&weights, layout);
        Self {
            field,
            keys,
            order,
            blocks,
        }
    }

    /// Calls `f(key_index, value)` for each entry of a range, in order.
    fn visit(&self, range: Range<usize>, mut f: impl FnMut(u32, &V)) {
        match &self.order {
            Order::Sorted(rows) => {
                for (k, r) in &rows[range] {
                    f(*k, r.value());
                }
            }
            Order::ByTable(present) => {
                for i in range.filter(|&i| present[i]) {
                    let r = self
                        .field
                        .get(&K::from_raw(self.keys.id_at(i)))
                        .expect("the graph is not mutated while it is written");
                    f(i as u32, r.value());
                }
            }
        }
    }

    /// One block per range. The first index region holds the key gaps;
    /// `encode(key_index, value, index_regions, data_regions)` appends the
    /// rest of one entry.
    fn blocks<'s, const I: usize, const D: usize, F>(
        &'s self,
        encode: F,
    ) -> impl IndexedParallelIterator<Item = RawBlock> + 's
    where
        F: Fn(u32, &V, &mut [Vec<u8>; I], &mut [Vec<u8>; D]) + Send + Sync + 's,
    {
        self.blocks.par_iter().map(move |(range, entries)| {
            let mut keys = Vec::new();
            let mut key_writer = KeyWriter::default();
            let mut index: [Vec<u8>; I] = std::array::from_fn(|_| Vec::new());
            let mut data: [Vec<u8>; D] = std::array::from_fn(|_| Vec::new());
            self.visit(range.clone(), |k, v| {
                key_writer.put(&mut keys, k);
                encode(k, v, &mut index, &mut data);
            });
            let mut index_regions = Vec::with_capacity(I + 1);
            index_regions.push(keys);
            index_regions.extend(index);
            RawBlock {
                entries: *entries,
                index: index_regions,
                data: data.into(),
            }
        })
    }
}

fn string_blocks<'t, K: RawId>(
    table: &'t Table<'_, K>,
    layout: Layout,
) -> impl IndexedParallelIterator<Item = RawBlock> + 't {
    table.refs.par_chunks(layout.strings).map(|chunk| {
        let mut prefix = Vec::new();
        let mut suffix_len = Vec::new();
        let mut bytes = Vec::new();
        let mut prev: &[u8] = &[];
        for r in chunk {
            let s = r.value().as_bytes();
            let p = prev
                .iter()
                .zip(s.iter())
                .take_while(|(a, b)| a == b)
                .count();
            put(&mut prefix, p as u64);
            put(&mut suffix_len, (s.len() - p) as u64);
            bytes.extend_from_slice(&s[p..]);
            prev = s;
        }
        RawBlock {
            entries: chunk.len() as u64,
            index: vec![prefix, suffix_len],
            data: vec![bytes],
        }
    })
}

/// Appends one edge list as on-disk value indices in `mode`.
fn put_list<V: RawId>(
    out: &mut Vec<u8>,
    key: u32,
    list: &[V],
    values: &Table<'_, V>,
    mode: ListMode,
) {
    let sorted = || -> Vec<u32> {
        if list.len() > HUB_LIST {
            let mut indices: Vec<u32> = list.par_iter().map(|v| values.index_of(v.raw())).collect();
            indices.par_sort_unstable();
            indices
        } else {
            let mut indices: Vec<u32> = list.iter().map(|v| values.index_of(v.raw())).collect();
            indices.sort_unstable();
            indices
        }
    };
    match mode {
        ListMode::Raw => {
            for v in list {
                put(out, values.index_of(v.raw()) as u64);
            }
        }
        ListMode::Abs32 => {
            for x in sorted() {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        ListMode::Gaps | ListMode::GapsRel => {
            let mut prev = 0u32;
            for (j, x) in sorted().into_iter().enumerate() {
                let v = match j {
                    0 if mode == ListMode::GapsRel => zigzag(x as i64 - key as i64),
                    0 => x as u64,
                    _ => (x - prev) as u64,
                };
                put(out, v);
                prev = x;
            }
        }
    }
}

// ---------- zstd ----------

/// Packs and compresses one block part.
fn compress_part(regions: &[Vec<u8>]) -> anyhow::Result<(Vec<u8>, u64)> {
    let raw = pack_regions(regions);
    anyhow::ensure!(
        raw.len() as u64 <= MAX_PART_BYTES,
        "a block part of {} bytes exceeds {MAX_PART_BYTES}",
        raw.len()
    );
    Ok((compress_frame(&raw)?, raw.len() as u64))
}

/// Compresses one frame with its content size and a checksum. Dense indices
/// make a flipped byte decode to another valid id, so corruption must be
/// caught before decoding. Each frame gets a fresh zstd context: a context
/// kept per thread would hold tens of MB on every pool thread for the life of
/// the process.
fn compress_frame(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut compressor =
        zstd::bulk::Compressor::new(ZSTD_LEVEL).context("creating a zstd compressor")?;
    compressor.include_checksum(true)?;
    compressor.include_contentsize(true)?;
    Ok(compressor.compress(raw)?)
}

/// The content size that a frame declares. The frame must also carry a
/// checksum, which decompression then verifies.
fn frame_size(comp: &[u8]) -> anyhow::Result<u64> {
    let size = zstd::zstd_safe::get_frame_content_size(comp)
        .ok()
        .flatten()
        .context("frame does not declare its size")?;
    anyhow::ensure!(
        comp.get(FRAME_DESCRIPTOR_BYTE)
            .is_some_and(|descriptor| descriptor & FRAME_CHECKSUM_FLAG != 0),
        "frame has no checksum"
    );
    Ok(size)
}

/// Decompresses a frame whose declared `size` the caller has checked.
fn decompress_frame(comp: &[u8], size: u64) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    out.try_reserve_exact(usize::try_from(size)?)?;
    let mut decompressor = zstd::bulk::Decompressor::new()?;
    decompressor.window_log_max(MAX_WINDOW_LOG)?;
    decompressor
        .decompress_to_buffer(comp, &mut out)
        .context("decompressing frame")?;
    anyhow::ensure!(out.len() as u64 == size, "frame length mismatch");
    Ok(out)
}

// ---------- writer: file ----------

/// Writes streams one at a time. Each stream's blocks are compressed in
/// parallel and written as soon as the stream is done, so at most one
/// stream's compressed bytes are held at once.
struct FileWriter<'w> {
    out: &'w mut dyn Write,
    offset: u64,
    layout: Layout,
    streams: Vec<Stream>,
}

impl<'w> FileWriter<'w> {
    fn new(out: &'w mut dyn Write, layout: Layout) -> anyhow::Result<Self> {
        out.write_all(&MAGIC)
            .and_then(|()| out.write_all(&SCHEMA_VERSION.to_le_bytes()))
            .context("writing the graph file header")?;
        Ok(Self {
            out,
            offset: HEADER_BYTES,
            layout,
            streams: Vec::new(),
        })
    }

    fn stream(
        &mut self,
        name: impl Into<String>,
        blocks: impl IndexedParallelIterator<Item = RawBlock>,
    ) -> anyhow::Result<()> {
        self.push(name.into(), None, blocks)
    }

    fn push(
        &mut self,
        name: String,
        mode: Option<ListMode>,
        blocks: impl IndexedParallelIterator<Item = RawBlock>,
    ) -> anyhow::Result<()> {
        let compressed = blocks
            .map(|b| Ok((b.entries, compress_part(&b.index)?, compress_part(&b.data)?)))
            .collect::<anyhow::Result<Vec<_>>>()
            .with_context(|| format!("encoding {name}"))?;
        let mut blocks = Vec::with_capacity(compressed.len());
        for (entries, index, data) in compressed {
            let index = self
                .append(index)
                .with_context(|| format!("writing {name}"))?;
            let data = self
                .append(data)
                .with_context(|| format!("writing {name}"))?;
            blocks.push(Block {
                entries,
                index,
                data,
            });
        }
        self.streams.push(Stream { name, mode, blocks });
        Ok(())
    }

    fn append(&mut self, (comp, uncomp): (Vec<u8>, u64)) -> anyhow::Result<Part> {
        self.out
            .write_all(&comp)
            .with_context(|| format!("writing a block part at offset {}", self.offset))?;
        let part = Part {
            offset: self.offset,
            comp: comp.len() as u64,
            uncomp,
        };
        self.offset += part.comp;
        Ok(part)
    }

    fn finish(self) -> anyhow::Result<()> {
        let dir = Directory {
            streams: self.streams,
        };
        let raw = bincode::serde::encode_to_vec(&dir, bincode::config::standard())
            .context("encoding the directory")?;
        let comp = compress_frame(&raw).context("compressing the directory")?;
        // The reader applies the limit to both sizes.
        anyhow::ensure!(
            raw.len().max(comp.len()) <= MAX_DIRECTORY_BYTES,
            "a directory of {} bytes ({} compressed) exceeds {MAX_DIRECTORY_BYTES}",
            raw.len(),
            comp.len()
        );
        decode_directory(&raw).context("the directory would not load")?;
        self.out
            .write_all(&comp)
            .and_then(|()| self.out.write_all(&self.offset.to_le_bytes()))
            .and_then(|()| self.out.write_all(&MAGIC))
            .and_then(|()| self.out.flush())
            .context("writing the directory and trailer")?;
        Ok(())
    }

    /// A string table, then its extras as raw ids. The extras index holds one
    /// zero varint per id, like every other stream's first index region.
    fn table<K: RawId>(&mut self, table: &Table<'_, K>) -> anyhow::Result<()> {
        self.stream(
            format!("str/{}", table.name),
            string_blocks(table, self.layout),
        )?;
        let ids = table
            .extras
            .iter()
            .flat_map(|id| id.to_le_bytes())
            .collect();
        let extras = RawBlock {
            entries: table.extras.len() as u64,
            index: vec![vec![0u8; table.extras.len()]],
            data: vec![ids],
        };
        self.stream(
            format!("extra/{}", table.name),
            vec![extras].into_par_iter(),
        )
    }

    fn list<K, V>(
        &mut self,
        name: &str,
        field: &ShardedField<IdDashMap<K, Vec<V>>>,
        keys: &Table<'_, K>,
        values: &Table<'_, V>,
        mode: ListMode,
    ) -> anyhow::Result<()>
    where
        K: RawId,
        V: RawId,
    {
        let rows = Rows::new(field, keys, self.layout, Vec::len);
        let blocks = rows.blocks::<1, 1, _>(|k, list, [counts], [out]| {
            put(counts, list.len() as u64);
            put_list(out, k, list, values, mode);
        });
        self.push(name.to_owned(), Some(mode), blocks)
    }
}

/// Writes `graph`. The graph must not be mutated while it is written.
pub(super) fn write(graph: &TargetGraph, writer: &mut dyn Write) -> anyhow::Result<()> {
    write_with(graph, writer, LAYOUT)
}

fn write_with(graph: &TargetGraph, writer: &mut dyn Write, layout: Layout) -> anyhow::Result<()> {
    // Naming every field makes adding one to `TargetGraph` a compile error
    // here until the layout covers it. `list_streams!` covers the lists.
    let TargetGraph {
        target_id_to_label: _,
        rule_type_id_to_string: _,
        oncall_id_to_string: _,
        label_id_to_string: _,
        minimized_targets,
        label_set_id_to_labels,
        glob_pattern_id_to_string: _,
        package_id_to_path: _,
        file_id_to_path: _,
        ci_deps_pattern_id_to_string: _,
        target_id_to_rdeps: _,
        target_id_to_deps: _,
        file_id_to_rdeps: _,
        file_id_to_deps: _,
        package_id_to_errors,
        package_id_to_targets: _,
        target_id_to_ci_srcs: _,
        target_id_to_ci_srcs_must_match: _,
        target_id_to_ci_deps_package_patterns: _,
        target_id_to_ci_deps_recursive_patterns: _,
        targets_with_sudo_label,
        ci_hint_to_affected: _,
        affected_to_ci_hints: _,
    } = graph;

    let dangling_label_sets = minimized_targets
        .par_iter()
        .filter(|e| !label_set_id_to_labels.contains_key(&e.value().label_set))
        .count();
    anyhow::ensure!(
        dangling_label_sets == 0,
        "{dangling_label_sets} targets refer to a label set the graph does not hold"
    );
    let bad_label_sets = label_set_id_to_labels
        .par_iter()
        .filter(|e| LabelSetId::of(e.value()) != *e.key())
        .count();
    anyhow::ensure!(
        bad_label_sets == 0,
        "{bad_label_sets} label set keys are not the hash of their labels"
    );

    let tables = Tables::build(graph)?;
    let mut out = FileWriter::new(writer, layout)?;
    out.table(&tables.target)?;
    out.table(&tables.rule)?;
    out.table(&tables.oncall)?;
    out.table(&tables.label)?;
    out.table(&tables.glob)?;
    out.table(&tables.package)?;
    out.table(&tables.file)?;
    out.table(&tables.cidep)?;

    // Label sets are sorted by their label indices. A target refers to a set
    // by its position in that order.
    let mut label_sets: Vec<(Vec<u32>, u64)> = label_set_id_to_labels
        .par_iter()
        .map(|e| {
            let labels = e.value().iter().map(|x| tables.label.index_of(x.raw()));
            (labels.collect(), e.key().raw())
        })
        .collect();
    label_sets.par_sort_unstable();
    out.stream(
        "labelsets",
        label_sets.par_chunks(layout.map_entries).map(|chunk| {
            let mut counts = Vec::new();
            let mut values = Vec::new();
            for (labels, _) in chunk {
                put(&mut counts, labels.len() as u64);
                for &x in labels {
                    put(&mut values, x as u64);
                }
            }
            RawBlock {
                entries: chunk.len() as u64,
                index: vec![counts],
                data: vec![values],
            }
        }),
    )?;
    let label_set_index = IdIndex::new(label_sets.par_iter().map(|x| x.1))?;
    drop(label_sets);

    let rows = Rows::new(minimized_targets, &tables.target, layout, |_| 1);
    let blocks = rows.blocks::<0, 6, _>(|_, m, [], [rule, oncall, label_set, tag, hash, text]| {
        put(rule, tables.rule.index_of(m.rule_type.raw()) as u64);
        let oncall_index = m
            .oncall
            .map_or(0, |x| tables.oncall.index_of(x.raw()) as u64 + 1);
        put(oncall, oncall_index);
        let set = label_set_index
            .get(m.label_set.raw())
            .expect("every label set was checked above");
        put(label_set, set as u64);
        match m.target_hash.as_hex128() {
            Some(b) => {
                tag.push(0);
                hash.extend_from_slice(b);
            }
            None => {
                tag.push(1);
                m.target_hash.with_str(|s| {
                    put(text, s.len() as u64);
                    text.extend_from_slice(s.as_bytes());
                });
            }
        }
    });
    out.stream("minimized", blocks)?;
    drop(rows);

    macro_rules! lists {
        ($(($name:literal, $field:ident, $k:ident, $v:ident, $mode:ident)),* $(,)?) => {$(
            out.list($name, &graph.$field, &tables.$k, &tables.$v, ListMode::$mode)?;
        )*};
    }
    list_streams!(lists);

    let mut sudo: Vec<u32> = targets_with_sudo_label
        .iter()
        .map(|k| tables.target.index_of(k.raw()))
        .collect();
    sudo.sort_unstable();
    out.stream(
        "sudo",
        sudo.par_chunks(layout.map_entries).map(|chunk| {
            let mut keys = Vec::new();
            let mut key_writer = KeyWriter::default();
            for &k in chunk {
                key_writer.put(&mut keys, k);
            }
            RawBlock {
                entries: chunk.len() as u64,
                index: vec![keys],
                data: Vec::new(),
            }
        }),
    )?;

    let rows = Rows::new(
        package_id_to_errors,
        &tables.package,
        layout,
        |m: &Vec<String>| m.iter().map(String::len).sum(),
    );
    let blocks = rows.blocks::<1, 1, _>(|_, v, [counts], [messages]| {
        put(counts, v.len() as u64);
        for s in v {
            put(messages, s.len() as u64);
            messages.extend_from_slice(s.as_bytes());
        }
    });
    out.stream("errors", blocks)?;
    drop(rows);
    out.finish()
}

// ---------- reader ----------

/// Fills `buf` from `offset` with positioned reads, so blocks of one file can
/// be read from many threads without a shared cursor.
#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        match std::os::windows::fs::FileExt::seek_read(file, buf, offset) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Decodes a directory and requires that it uses every byte. The limit caps
/// what bincode allocates for the lengths that the directory claims. Bincode
/// counts a u64 as 8 bytes against it, however short its encoding, so the
/// writer decodes each directory with this function before writing it.
fn decode_directory(raw: &[u8]) -> anyhow::Result<Directory> {
    let (dir, used): (Directory, _) = bincode::serde::decode_from_slice(
        raw,
        bincode::config::standard().with_limit::<MAX_DIRECTORY_BYTES>(),
    )?;
    anyhow::ensure!(
        used == raw.len(),
        "{} bytes follow the directory",
        raw.len() - used
    );
    Ok(dir)
}

/// Checks whether the read was cancelled. The reader takes a trait object so
/// that it is compiled once, here, and not again in every calling crate.
type CheckCancelled<'a> = dyn Fn() -> anyhow::Result<()> + Sync + 'a;

/// Reads the header, trailer and directory. Checks that the block parts tile
/// the data region and that no part claims more than `MAX_PART_BYTES`.
fn read_directory(file: &File) -> anyhow::Result<Directory> {
    let mut head = [0u8; HEADER_BYTES as usize];
    match read_exact_at(file, &mut head, 0) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            anyhow::bail!("graph file too small: shorter than its {HEADER_BYTES}-byte header")
        }
        Err(e) => return Err(e).context("reading graph file header"),
    }
    let (magic, version) = head.split_at(MAGIC.len());
    if magic != MAGIC {
        anyhow::bail!("graph file magic mismatch: expected {MAGIC:?}, got {magic:?}");
    }
    let version = u32::from_le_bytes(version.try_into().expect("4-byte version"));
    if version != SCHEMA_VERSION {
        // Callers outside this crate classify a schema mismatch by this exact
        // text, which every earlier layout also used.
        anyhow::bail!("framed file version mismatch: expected {SCHEMA_VERSION}, got {version}");
    }
    let len = file
        .metadata()
        .context("reading graph file metadata")?
        .len();
    anyhow::ensure!(
        len >= HEADER_BYTES + TRAILER_BYTES,
        "graph file too small ({len} bytes, need >= {})",
        HEADER_BYTES + TRAILER_BYTES
    );
    let mut tail = [0u8; TRAILER_BYTES as usize];
    read_exact_at(file, &mut tail, len - TRAILER_BYTES).context("reading graph file trailer")?;
    let (dir_offset, magic) = tail.split_at(size_of::<u64>());
    anyhow::ensure!(magic == MAGIC, "graph file trailer magic mismatch");
    let dir_offset = u64::from_le_bytes(dir_offset.try_into().expect("8-byte offset"));
    anyhow::ensure!(
        (HEADER_BYTES..=len - TRAILER_BYTES).contains(&dir_offset),
        "directory offset {dir_offset} out of range"
    );
    let dir_len = usize::try_from(len - TRAILER_BYTES - dir_offset)?;
    anyhow::ensure!(
        dir_len <= MAX_DIRECTORY_BYTES,
        "directory of {dir_len} bytes exceeds {MAX_DIRECTORY_BYTES}"
    );
    let mut comp = vec![0u8; dir_len];
    read_exact_at(file, &mut comp, dir_offset).context("reading directory")?;
    let size = frame_size(&comp).context("reading directory")?;
    anyhow::ensure!(
        size <= MAX_DIRECTORY_BYTES as u64,
        "directory of {size} bytes exceeds {MAX_DIRECTORY_BYTES}"
    );
    let raw = decompress_frame(&comp, size).context("reading directory")?;
    let dir = decode_directory(&raw).context("decoding directory")?;

    let mut parts = Vec::new();
    for s in &dir.streams {
        for (i, b) in s.blocks.iter().enumerate() {
            for part in [&b.index, &b.data] {
                anyhow::ensure!(
                    part.comp > 0 && part.uncomp <= MAX_PART_BYTES,
                    "{} block {i}: a part of {} bytes claims {} uncompressed bytes",
                    s.name,
                    part.comp,
                    part.uncomp
                );
                parts.push((part.offset, part.comp));
            }
        }
    }
    // The writer lays parts out back to back, so each byte of the data region
    // belongs to exactly one part.
    parts.sort_unstable();
    let mut end = HEADER_BYTES;
    for (offset, comp) in parts {
        anyhow::ensure!(
            offset == end,
            "blocks do not tile the data region: expected a part at {end}, found {offset}"
        );
        end = offset
            .checked_add(comp)
            .context("block part extends past the end of the file")?;
    }
    anyhow::ensure!(
        end == dir_offset,
        "blocks end at {end}, but the directory starts at {dir_offset}"
    );
    Ok(dir)
}

/// Reads and decompresses one block part. The zstd frame header must declare
/// the size the directory claims, so a corrupt directory cannot make this
/// allocate more than the frame produces.
fn read_part(file: &File, part: &Part) -> anyhow::Result<Vec<u8>> {
    // The part lies inside the file, so the file bounds this allocation.
    let mut comp = vec![0u8; usize::try_from(part.comp)?];
    read_exact_at(file, &mut comp, part.offset).context("reading block")?;
    let declared = frame_size(&comp).context("reading block")?;
    anyhow::ensure!(
        declared == part.uncomp,
        "block frame declares {declared} bytes, the directory claims {}",
        part.uncomp
    );
    decompress_frame(&comp, declared).context("reading block")
}

/// What every stream reader needs: the file, its directory, and the caller's
/// cancellation check, which runs before every block.
#[derive(Clone, Copy)]
struct Reader<'a> {
    file: &'a File,
    dir: &'a Directory,
    check_cancelled: &'a CheckCancelled<'a>,
}

/// A stream whose index parts are decompressed and whose entry counts match
/// them, so `entries` can size allocations.
struct Checked<'a> {
    reader: Reader<'a>,
    stream: &'a Stream,
    indexes: Vec<Vec<u8>>,
}

impl<'a> Reader<'a> {
    fn stream(self, name: &str) -> anyhow::Result<Checked<'a>> {
        let stream = self.dir.stream(name)?;
        let indexes = stream
            .blocks
            .par_iter()
            .enumerate()
            .map(|(i, b)| {
                (self.check_cancelled)()?;
                let index = read_part(self.file, &b.index)?;
                // Each entry has exactly one varint in the first region, so
                // counting the bytes that end a varint counts the entries.
                let first = first_region(&index)?;
                let found = first.iter().filter(|&&byte| byte < 0x80).count() as u64;
                anyhow::ensure!(
                    found == b.entries && first.last().is_none_or(|&byte| byte < 0x80),
                    "block {i}: the index holds {found} entries, the directory claims {}",
                    b.entries
                );
                Ok(index)
            })
            .collect::<anyhow::Result<Vec<_>>>()
            .with_context(|| format!("reading {name}"))?;
        Ok(Checked {
            reader: self,
            stream,
            indexes,
        })
    }

    /// The raw ids of a table's extras.
    fn extras(self, name: &str) -> anyhow::Result<Vec<u64>> {
        let stream = self.stream(&format!("extra/{name}"))?;
        let mut ids = vec![0u64; stream.entries()];
        stream.decode_blocks(&mut ids, |index, data, slot| {
            let zeros = cursors(index, 1)?;
            anyhow::ensure!(zeros[0].b.iter().all(|&b| b == 0), "malformed extras index");
            let mut data = cursors(data, 1)?;
            let ids = &mut data[0];
            for id in slot.iter_mut() {
                *id = u64::from_le_bytes(ids.array::<8>()?);
            }
            anyhow::ensure!(ids.done(), "trailing bytes");
            Ok(())
        })?;
        Ok(ids)
    }

    /// Decodes a string table into its id array (strings, then extras) and the
    /// id -> string map.
    fn table<K: RawId>(
        self,
        name: &str,
    ) -> anyhow::Result<(Vec<u64>, ShardedField<IdDashMap<K, String>>)> {
        let extras = self.extras(name)?;
        let stream = self.stream(&format!("str/{name}"))?;
        let field = new_field::<K, String>(stream.entries());
        let mut ids = Vec::with_capacity(stream.entries() + extras.len());
        ids.resize(stream.entries(), 0u64);
        stream.decode_blocks(&mut ids, |index, data, slot| {
            let mut index = cursors(index, 2)?;
            let mut data = cursors(data, 1)?;
            let [prefix, suffix] = index.as_mut_slice() else {
                unreachable!("cursors returned two regions")
            };
            let bytes = &mut data[0];
            let mut prev: Vec<u8> = Vec::new();
            for id_slot in slot.iter_mut() {
                let p = prefix.index()?;
                anyhow::ensure!(
                    p <= prev.len(),
                    "shared prefix longer than the previous string"
                );
                let n = suffix.index()?;
                prev.truncate(p);
                prev.extend_from_slice(bytes.bytes(n)?);
                let string = String::from_utf8(prev.clone()).context("string is not UTF-8")?;
                let id: K = id_of(&string);
                *id_slot = id.raw();
                field.insert(id, string);
            }
            anyhow::ensure!(all_done(&[&index, &data]), "trailing bytes");
            Ok(())
        })?;
        stream.check_distinct(field.len())?;
        ids.extend(extras);
        Ok((ids, field))
    }

    /// Decodes an edge list with the list mode that the directory stores.
    fn list<K: RawId, V: RawId>(
        self,
        name: &str,
        key_ids: &[u64],
        value_ids: &[u64],
    ) -> anyhow::Result<ShardedField<IdDashMap<K, Vec<V>>>> {
        let stream = self.stream(name)?;
        let mode = stream
            .stream
            .mode
            .with_context(|| format!("{name} has no list mode"))?;
        let field = new_field::<K, Vec<V>>(stream.entries());
        stream.decode_entries(1, 1, |key, index, data| {
            let values = &mut data[0];
            let n = index[0].count(values)?;
            let mut list = Vec::with_capacity(n);
            let mut prev = 0u64;
            for j in 0..n {
                let i = match mode {
                    ListMode::Raw => values.get()?,
                    ListMode::Abs32 => u32::from_le_bytes(values.array::<4>()?) as u64,
                    ListMode::Gaps | ListMode::GapsRel => {
                        let x = values.get()?;
                        let i = if j > 0 {
                            prev.checked_add(x)
                        } else if mode == ListMode::GapsRel {
                            i64::try_from(key)
                                .ok()
                                .and_then(|k| k.checked_add(unzigzag(x)))
                                .and_then(|i| u64::try_from(i).ok())
                        } else {
                            Some(x)
                        };
                        let i = i.context("value index overflows")?;
                        prev = i;
                        i
                    }
                };
                list.push(V::from_raw(at(value_ids, i)?));
            }
            field.insert(K::from_raw(at(key_ids, key as u64)?), list);
            Ok(())
        })?;
        stream.check_distinct(field.len())?;
        Ok(field)
    }

    /// Decodes the label sets into their ids in on-disk order and the id ->
    /// labels map.
    fn label_sets(
        self,
        label_ids: &[u64],
    ) -> anyhow::Result<(Vec<u64>, ShardedField<IdDashMap<LabelSetId, Vec<LabelId>>>)> {
        let stream = self.stream("labelsets")?;
        let field = new_field::<LabelSetId, Vec<LabelId>>(stream.entries());
        let mut ids = vec![0u64; stream.entries()];
        stream.decode_blocks(&mut ids, |index, data, slot| {
            let mut index = cursors(index, 1)?;
            let mut data = cursors(data, 1)?;
            let (counts, values) = (&mut index[0], &mut data[0]);
            for id_slot in slot.iter_mut() {
                let n = counts.count(values)?;
                let mut labels = Vec::with_capacity(n);
                for _ in 0..n {
                    labels.push(LabelId::from_raw(at(label_ids, values.get()?)?));
                }
                let id = LabelSetId::of(&labels);
                *id_slot = id.raw();
                field.insert(id, labels);
            }
            anyhow::ensure!(all_done(&[&index, &data]), "trailing bytes");
            Ok(())
        })?;
        stream.check_distinct(field.len())?;
        Ok((ids, field))
    }

    fn minimized(
        self,
        ids: &Ids,
        label_set_ids: &[u64],
    ) -> anyhow::Result<ShardedField<IdDashMap<TargetId, StoredMinimizedTarget>>> {
        let stream = self.stream("minimized")?;
        let field = new_field::<TargetId, StoredMinimizedTarget>(stream.entries());
        stream.decode_entries(0, 6, |key, _, data| {
            let [rules, oncalls, label_sets, tags, hashes, texts] = data else {
                unreachable!("cursors returned six regions")
            };
            let rule_type = RuleTypeId::from_raw(at(&ids.rule, rules.get()?)?);
            let oncall = match oncalls.get()? {
                0 => None,
                x => Some(OncallId::from_raw(at(&ids.oncall, x - 1)?)),
            };
            let label_set = LabelSetId::from_raw(at(label_set_ids, label_sets.get()?)?);
            let target_hash = match tags.bytes(1)?[0] {
                0 => TargetHash::from_hex128(hashes.array::<16>()?),
                1 => {
                    let n = texts.index()?;
                    let text =
                        std::str::from_utf8(texts.bytes(n)?).context("target hash is not UTF-8")?;
                    TargetHash::new(text)
                }
                t => anyhow::bail!("unknown hash tag {t}"),
            };
            field.insert(
                TargetId::from_raw(at(&ids.target, key as u64)?),
                StoredMinimizedTarget {
                    rule_type,
                    oncall,
                    label_set,
                    target_hash,
                },
            );
            Ok(())
        })?;
        stream.check_distinct(field.len())?;
        Ok(field)
    }

    fn sudo(self, target_ids: &[u64]) -> anyhow::Result<ShardedField<IdDashSet<TargetId>>> {
        let stream = self.stream("sudo")?;
        let field = ShardedField::<IdDashSet<TargetId>>::new(pick_shard_count(stream.entries()));
        stream.decode_entries(0, 0, |key, _, _| {
            field.insert(TargetId::from_raw(at(target_ids, key as u64)?));
            Ok(())
        })?;
        stream.check_distinct(field.len())?;
        Ok(field)
    }

    fn errors(
        self,
        package_ids: &[u64],
    ) -> anyhow::Result<ShardedField<IdDashMap<PackageId, Vec<String>>>> {
        let stream = self.stream("errors")?;
        let field = new_field::<PackageId, Vec<String>>(stream.entries());
        stream.decode_entries(1, 1, |key, index, data| {
            let messages = &mut data[0];
            let n = index[0].count(messages)?;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                let len = messages.index()?;
                let message =
                    std::str::from_utf8(messages.bytes(len)?).context("message is not UTF-8")?;
                v.push(message.to_owned());
            }
            field.insert(PackageId::from_raw(at(package_ids, key as u64)?), v);
            Ok(())
        })?;
        stream.check_distinct(field.len())?;
        Ok(field)
    }
}

impl Checked<'_> {
    fn entries(&self) -> usize {
        self.stream.blocks.iter().map(|b| b.entries as usize).sum()
    }

    /// Every entry has its own key, so a decoded field holds one entry per
    /// key. Fewer means two blocks repeated a key and one overwrote the other.
    fn check_distinct(&self, decoded: usize) -> anyhow::Result<()> {
        anyhow::ensure!(
            decoded == self.entries(),
            "{}: {decoded} distinct keys for {} entries",
            self.stream.name,
            self.entries()
        );
        Ok(())
    }

    /// Decodes every block in parallel. `decode(index, data, slots)` gets the
    /// slice of `slots` for the block's entries.
    fn decode_blocks<T: Send>(
        &self,
        slots: &mut [T],
        decode: impl Fn(&[u8], &[u8], &mut [T]) -> anyhow::Result<()> + Sync,
    ) -> anyhow::Result<()> {
        let mut rest = slots;
        let mut per_block = Vec::with_capacity(self.stream.blocks.len());
        for b in &self.stream.blocks {
            let (head, tail) = rest.split_at_mut(b.entries as usize);
            per_block.push(head);
            rest = tail;
        }
        per_block
            .into_par_iter()
            .zip(self.stream.blocks.par_iter().zip(self.indexes.par_iter()))
            .enumerate()
            .try_for_each(|(i, (slot, (b, index)))| {
                (self.reader.check_cancelled)()?;
                let data = read_part(self.reader.file, &b.data)?;
                decode(index, &data, slot).with_context(|| format!("block {i}"))
            })
            .with_context(|| format!("decoding {}", self.stream.name))
    }

    /// Decodes every entry of a keyed stream. The first index region holds
    /// the key gaps; `entry(key_index, index_regions, data_regions)` decodes
    /// the rest of one entry.
    fn decode_entries(
        &self,
        index_regions: usize,
        data_regions: usize,
        entry: impl Fn(usize, &mut [Cursor<'_>], &mut [Cursor<'_>]) -> anyhow::Result<()> + Sync,
    ) -> anyhow::Result<()> {
        let mut slots = vec![(); self.entries()];
        self.decode_blocks(&mut slots, |index, data, slot| {
            let mut index = cursors(index, 1 + index_regions)?;
            let mut data = cursors(data, data_regions)?;
            let (key_gaps, index) = index.split_first_mut().expect("the key region");
            let mut keys = Keys::default();
            for _ in slot.iter() {
                let key = keys.next(key_gaps)?;
                entry(key, index, &mut data)?;
            }
            anyhow::ensure!(
                key_gaps.done() && all_done(&[&*index, &data]),
                "trailing bytes"
            );
            Ok(())
        })
    }
}

/// The first region of a block part.
fn first_region(part: &[u8]) -> anyhow::Result<&[u8]> {
    let mut c = Cursor::new(part);
    let n = c.index()?;
    anyhow::ensure!(n >= 1, "block has no regions");
    let first = c.index()?;
    for _ in 1..n {
        c.index()?;
    }
    c.bytes(first)
}

fn new_field<K: ShardKey, V>(entries: usize) -> ShardedField<IdDashMap<K, V>> {
    let n = pick_shard_count(entries);
    ShardedField::from_shards(
        (0..n)
            .map(|_| IdDashMap::with_capacity_and_hasher(entries / n + 1, BuildNoHash::default()))
            .collect(),
    )
}

/// Reads a graph written by `write`, checking `check_cancelled` before every
/// block.
pub(super) fn read(
    file: &File,
    check_cancelled: &CheckCancelled<'_>,
) -> anyhow::Result<TargetGraph> {
    check_cancelled()?;
    let dir = read_directory(file)?;
    let r = Reader {
        file,
        dir: &dir,
        check_cancelled,
    };

    let (target, rest) = rayon::join(
        || r.table::<TargetId>("target"),
        || {
            Ok::<_, anyhow::Error>((
                r.table::<RuleTypeId>("rule")?,
                r.table::<OncallId>("oncall")?,
                r.table::<LabelId>("label")?,
                r.table::<GlobPatternId>("glob")?,
                r.table::<PackageId>("package")?,
                r.table::<FileId>("file")?,
                r.table::<CiDepsPatternId>("cidep")?,
            ))
        },
    );
    let (target, target_id_to_label) = target?;
    let (
        (rule, rule_type_id_to_string),
        (oncall, oncall_id_to_string),
        (label, label_id_to_string),
        (glob, glob_pattern_id_to_string),
        (package, package_id_to_path),
        (file_ids, file_id_to_path),
        (cidep, ci_deps_pattern_id_to_string),
    ) = rest?;
    let ids = Ids {
        target,
        rule,
        oncall,
        label,
        glob,
        package,
        file: file_ids,
        cidep,
    };

    // Every other stream depends only on the tables, so all of them decode at
    // once. Minimized targets also need the label sets.
    let mut sets_and_targets = None;
    let mut targets_with_sudo_label = None;
    let mut package_id_to_errors = None;
    macro_rules! read_lists {
        ($(($name:literal, $field:ident, $k:ident, $v:ident, $mode:ident)),* $(,)?) => {{
            $(let mut $field = None;)*
            rayon::scope(|s| {
                $(s.spawn(|_| $field = Some(r.list($name, &ids.$k, &ids.$v)));)*
                s.spawn(|_| {
                    sets_and_targets = Some(r.label_sets(&ids.label).and_then(
                        |(set_ids, sets)| Ok((sets, r.minimized(&ids, &set_ids)?)),
                    ));
                });
                s.spawn(|_| targets_with_sudo_label = Some(r.sudo(&ids.target)));
                s.spawn(|_| package_id_to_errors = Some(r.errors(&ids.package)));
            });
            let (label_set_id_to_labels, minimized_targets) =
                sets_and_targets.expect("spawned above")?;
            TargetGraph {
                target_id_to_label,
                rule_type_id_to_string,
                oncall_id_to_string,
                label_id_to_string,
                minimized_targets,
                label_set_id_to_labels,
                glob_pattern_id_to_string,
                package_id_to_path,
                file_id_to_path,
                ci_deps_pattern_id_to_string,
                package_id_to_errors: package_id_to_errors.expect("spawned above")?,
                targets_with_sudo_label: targets_with_sudo_label.expect("spawned above")?,
                $($field: $field.expect("spawned above")?,)*
            }
        }};
    }
    let graph = list_streams!(read_lists);
    check_cancelled()?;
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::panic::AssertUnwindSafe;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use rstest::rstest;

    use super::*;

    const TINY: Layout = Layout {
        map_entries: 2,
        strings: 3,
        values: 4,
    };

    fn target(label: &str) -> TargetId {
        label.parse().expect("id parsing is infallible")
    }

    /// Exercises every field: hub and duplicate edges, ids without a string in
    /// every id space, a target hash that is not 32 hex characters, non-ASCII
    /// strings, order-sensitive lists, sudo, errors, ci_hint edges and file
    /// imports.
    fn sample_graph() -> TargetGraph {
        let g = TargetGraph::new();
        let rule = g.store_rule_type("prelude//rules.bzl:cxx_library");
        let hint_rule = g.store_rule_type(&format!("prelude//rules.bzl:{CI_HINT_RULE_TYPE}"));
        let oncall = g.store_oncall("team_ü");
        let labels = [g.store_label("ci:diff"), g.store_label("ci:land")];
        let pkg = g.store_package("fbcode//a");
        let other_pkg = g.store_package("fbcode//é");
        let globs = [
            g.store_glob_pattern("src/**"),
            g.store_glob_pattern("include/**"),
        ];
        let patterns = [
            g.store_ci_deps_pattern("fbcode//b"),
            g.store_ci_deps_pattern("fbcode//c"),
        ];
        let targets: Vec<TargetId> = (0..40)
            .map(|i| g.store_target(&format!("fbcode//a:t{i:02}")))
            .collect();
        let unicode = g.store_target("fbcode//é:ñ");
        let hint = g.store_target("fbcode//a:hint");
        for (i, &t) in targets.iter().chain([&unicode]).enumerate() {
            g.add_target_to_package(if t == unicode { other_pkg } else { pkg }, t);
            g.store_minimized_target(
                t,
                MinimizedBuckTarget {
                    rule_type: rule,
                    oncall: (i % 3 != 0).then_some(oncall),
                    labels: if i % 2 == 0 {
                        labels.to_vec()
                    } else {
                        vec![labels[1]]
                    },
                    target_hash: TargetHash::new(&format!("{i:032x}")),
                },
            );
        }
        g.add_target_to_package(pkg, hint);
        g.store_minimized_target(
            hint,
            MinimizedBuckTarget {
                rule_type: hint_rule,
                oncall: None,
                labels: Vec::new(),
                target_hash: TargetHash::new("not-a-hex-hash"),
            },
        );
        // A hub everything depends on, a chain, and a duplicate edge.
        for &t in &targets[1..] {
            g.store_deps_with_rdeps(t, vec![targets[0]]);
        }
        for pair in targets.windows(2) {
            g.store_deps_with_rdeps(pair[1], vec![pair[0]]);
        }
        g.store_deps_with_rdeps(targets[5], vec![targets[4]]);
        g.add_ci_hint_edge(hint, targets[3]);
        g.mark_target_has_sudo_label(targets[2]);
        g.add_error(other_pkg, "boom".to_owned());
        g.add_error(other_pkg, "second ✓".to_owned());
        g.add_ci_src(targets[1], globs[1]);
        g.add_ci_src(targets[1], globs[0]);
        g.add_ci_src_must_match(targets[1], globs[0]);
        g.add_ci_deps_package_pattern(targets[2], patterns[1]);
        g.add_ci_deps_package_pattern(targets[2], patterns[0]);
        g.add_ci_deps_recursive_pattern(targets[3], patterns[0]);
        let files: Vec<FileId> = (0..4)
            .map(|i| g.store_file(&format!("fbcode/a/defs{i}.bzl")))
            .collect();
        g.add_file_import(files[0], files[1]);
        g.add_file_import(files[0], files[2]);
        g.add_file_import(files[3], files[2]);

        // Ids that no string defines, in every id space.
        let ghost = target("fbcode//ghost:t");
        g.store_minimized_target(
            targets[7],
            MinimizedBuckTarget {
                rule_type: "ghost_rule".parse().unwrap(),
                oncall: Some("ghost_oncall".parse().unwrap()),
                labels: vec!["ghost_label".parse().unwrap()],
                target_hash: TargetHash::new(&format!("{:032x}", 7)),
            },
        );
        g.store_deps_with_rdeps(unicode, vec![targets[7], ghost]);
        g.add_ci_src(targets[8], "ghost/**".parse().unwrap());
        g.add_target_to_package("fbcode//ghost".parse().unwrap(), ghost);
        g.add_file_import(
            "ghost/a.bzl".parse().unwrap(),
            "ghost/b.bzl".parse().unwrap(),
        );
        g.add_ci_deps_recursive_pattern(targets[9], "fbcode//ghost/...".parse().unwrap());
        g.add_error("fbcode//ghost_pkg".parse().unwrap(), "lost".to_owned());
        g.mark_target_has_sudo_label(ghost);
        g
    }

    /// Every field in a canonical form. Unordered edge lists are sorted, so
    /// they compare as multisets; ordered lists keep their order.
    #[derive(Debug, PartialEq)]
    struct Snapshot {
        strings: Vec<BTreeMap<u64, String>>,
        minimized: BTreeMap<u64, String>,
        label_sets: BTreeMap<u64, Vec<u64>>,
        unordered: Vec<BTreeMap<u64, Vec<u64>>>,
        ordered: Vec<BTreeMap<u64, Vec<u64>>>,
        errors: BTreeMap<u64, Vec<String>>,
        sudo: BTreeSet<u64>,
    }

    fn strings<K: RawId>(f: &ShardedField<IdDashMap<K, String>>) -> BTreeMap<u64, String> {
        f.iter()
            .map(|e| (e.key().raw(), e.value().clone()))
            .collect()
    }

    fn lists<K: RawId, V: RawId>(
        f: &ShardedField<IdDashMap<K, Vec<V>>>,
        sorted: bool,
    ) -> BTreeMap<u64, Vec<u64>> {
        f.iter()
            .map(|e| {
                let mut v: Vec<u64> = e.value().iter().map(|x| x.raw()).collect();
                if sorted {
                    v.sort_unstable();
                }
                (e.key().raw(), v)
            })
            .collect()
    }

    fn snapshot(g: &TargetGraph) -> Snapshot {
        // Naming every field makes a new field a compile error here.
        let TargetGraph {
            target_id_to_label,
            rule_type_id_to_string,
            oncall_id_to_string,
            label_id_to_string,
            minimized_targets,
            label_set_id_to_labels,
            glob_pattern_id_to_string,
            package_id_to_path,
            file_id_to_path,
            ci_deps_pattern_id_to_string,
            target_id_to_rdeps,
            target_id_to_deps,
            file_id_to_rdeps,
            file_id_to_deps,
            package_id_to_errors,
            package_id_to_targets,
            target_id_to_ci_srcs,
            target_id_to_ci_srcs_must_match,
            target_id_to_ci_deps_package_patterns,
            target_id_to_ci_deps_recursive_patterns,
            targets_with_sudo_label,
            ci_hint_to_affected,
            affected_to_ci_hints,
        } = g;
        Snapshot {
            strings: vec![
                strings(target_id_to_label),
                strings(rule_type_id_to_string),
                strings(oncall_id_to_string),
                strings(label_id_to_string),
                strings(glob_pattern_id_to_string),
                strings(package_id_to_path),
                strings(file_id_to_path),
                strings(ci_deps_pattern_id_to_string),
            ],
            minimized: minimized_targets
                .iter()
                .map(|e| (e.key().raw(), format!("{:?}", e.value())))
                .collect(),
            label_sets: label_set_id_to_labels
                .iter()
                .map(|e| (e.key().raw(), e.value().iter().map(|x| x.raw()).collect()))
                .collect(),
            unordered: vec![
                lists(target_id_to_deps, true),
                lists(target_id_to_rdeps, true),
                lists(package_id_to_targets, true),
                lists(file_id_to_deps, true),
                lists(file_id_to_rdeps, true),
                lists(ci_hint_to_affected, true),
                lists(affected_to_ci_hints, true),
            ],
            ordered: vec![
                lists(target_id_to_ci_srcs, false),
                lists(target_id_to_ci_srcs_must_match, false),
                lists(target_id_to_ci_deps_package_patterns, false),
                lists(target_id_to_ci_deps_recursive_patterns, false),
            ],
            errors: package_id_to_errors
                .iter()
                .map(|e| (e.key().raw(), e.value().clone()))
                .collect(),
            sudo: targets_with_sudo_label.iter().map(|k| k.raw()).collect(),
        }
    }

    fn encode(graph: &TargetGraph, layout: Layout) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_with(graph, &mut bytes, layout).expect("write");
        bytes
    }

    fn file_with(bytes: &[u8]) -> File {
        let mut file = tempfile::tempfile().expect("tempfile");
        file.write_all(bytes).expect("write");
        file
    }

    /// Decodes `bytes`, failing the test if the reader panics.
    fn decode(bytes: &[u8]) -> anyhow::Result<TargetGraph> {
        let file = file_with(bytes);
        std::panic::catch_unwind(AssertUnwindSafe(|| TargetGraph::read_framed(&file)))
            .unwrap_or_else(|_| panic!("the reader panicked"))
    }

    #[rstest]
    #[case::default_layout(LAYOUT)]
    #[case::tiny_blocks(TINY)]
    fn round_trip_preserves_every_field(#[case] layout: Layout) {
        let graph = sample_graph();
        let loaded = decode(&encode(&graph, layout)).expect("read");
        assert_eq!(snapshot(&loaded), snapshot(&graph));
    }

    #[test]
    fn unordered_lists_load_in_label_order() {
        let graph = sample_graph();
        let hub = target("fbcode//a:t00");
        let loaded = decode(&encode(&graph, TINY)).expect("read");
        let labels: Vec<String> = loaded
            .get_rdeps(hub)
            .expect("rdeps")
            .into_iter()
            .map(|t| loaded.get_target_label(t).expect("label"))
            .collect();
        let mut sorted = labels.clone();
        sorted.sort();
        assert_eq!(labels, sorted);
    }

    #[test]
    fn empty_graph_round_trips() {
        let graph = TargetGraph::new();
        let loaded = decode(&encode(&graph, LAYOUT)).expect("read");
        assert!(loaded.is_empty());
        assert_eq!(snapshot(&loaded), snapshot(&graph));
    }

    #[test]
    fn encoding_a_graph_or_its_reload_gives_the_same_bytes() {
        let graph = sample_graph();
        let bytes = encode(&graph, TINY);
        assert_eq!(bytes, encode(&graph, TINY));
        assert_eq!(bytes, encode(&decode(&bytes).expect("read"), TINY));
    }

    #[test]
    fn a_target_with_an_unknown_label_set_is_a_write_error() {
        let graph = sample_graph();
        let t = target("fbcode//a:t01");
        let mut stored = graph.minimized_targets.get(&t).expect("target").clone();
        stored.label_set = LabelSetId(12345);
        graph.minimized_targets.insert(t, stored);
        let err = write_with(&graph, &mut Vec::new(), LAYOUT).expect_err("must fail");
        assert!(format!("{err:#}").contains("label set"), "{err:#}");
    }

    #[test]
    fn read_honors_cancellation() {
        let file = file_with(&encode(&sample_graph(), TINY));
        let checks = AtomicUsize::new(0);
        let err = read(&file, &|| {
            if checks.fetch_add(1, Ordering::Relaxed) >= 5 {
                anyhow::bail!("cancelled by reader owner");
            }
            Ok(())
        })
        .expect_err("cancelled read fails");
        assert!(
            format!("{err:#}").contains("cancelled by reader owner"),
            "{err:#}"
        );
    }

    #[rstest]
    #[case::too_small(&|b: &mut Vec<u8>| b.truncate(4), "graph file too small: shorter than its 8-byte header")]
    #[case::header_without_trailer(&|b: &mut Vec<u8>| b.truncate(12), "graph file too small (12 bytes, need >= 20)")]
    #[case::header_magic(&|b: &mut Vec<u8>| b[0] ^= 1, "graph file magic mismatch")]
    #[case::trailer_magic(&|b: &mut Vec<u8>| *b.last_mut().unwrap() ^= 1, "graph file trailer magic mismatch")]
    fn a_damaged_header_or_trailer_is_rejected(
        #[case] damage: &dyn Fn(&mut Vec<u8>),
        #[case] message: &str,
    ) {
        let mut bytes = encode(&sample_graph(), TINY);
        damage(&mut bytes);
        let err = decode(&bytes).expect_err("a damaged file must not load");
        assert!(format!("{err:#}").starts_with(message), "{err:#}");
    }

    #[test]
    fn other_schema_reports_the_shared_version_message() {
        let mut bytes = encode(&sample_graph(), LAYOUT);
        bytes[4..8].copy_from_slice(&(SCHEMA_VERSION - 1).to_le_bytes());
        let err = decode(&bytes).expect_err("other schema must not load");
        assert_eq!(
            format!("{err:#}"),
            format!(
                "framed file version mismatch: expected {SCHEMA_VERSION}, got {}",
                SCHEMA_VERSION - 1
            )
        );
    }

    #[test]
    fn truncated_files_are_rejected() {
        let bytes = encode(&sample_graph(), TINY);
        for len in (0..bytes.len()).step_by(7) {
            assert!(
                decode(&bytes[..len]).is_err(),
                "a {len}-byte prefix must not load"
            );
        }
    }

    /// Lays the file out again with `edit_dir` applied to the directory and
    /// `edit_part(n, bytes)` applied to the decompressed bytes of each part.
    fn relayout(
        bytes: &[u8],
        edit_part: &dyn Fn(usize, &mut Vec<u8>),
        edit_dir: &dyn Fn(&mut Directory),
    ) -> Vec<u8> {
        let mut dir = read_directory(&file_with(bytes)).expect("directory");
        let mut out = bytes[..HEADER_BYTES as usize].to_vec();
        let mut n = 0;
        for s in &mut dir.streams {
            for b in &mut s.blocks {
                for part in [&mut b.index, &mut b.data] {
                    let start = part.offset as usize;
                    let comp = &bytes[start..start + part.comp as usize];
                    let mut raw = zstd::decode_all(comp).expect("decompress");
                    edit_part(n, &mut raw);
                    n += 1;
                    let comp = compress_frame(&raw).expect("compress");
                    *part = Part {
                        offset: out.len() as u64,
                        comp: comp.len() as u64,
                        uncomp: raw.len() as u64,
                    };
                    out.extend_from_slice(&comp);
                }
            }
        }
        edit_dir(&mut dir);
        let dir_offset = out.len() as u64;
        let raw = bincode::serde::encode_to_vec(&dir, bincode::config::standard()).unwrap();
        out.extend(compress_frame(&raw).expect("compress"));
        out.extend_from_slice(&dir_offset.to_le_bytes());
        out.extend_from_slice(&MAGIC);
        out
    }

    fn part_count(bytes: &[u8]) -> usize {
        read_directory(&file_with(bytes))
            .expect("directory")
            .streams
            .iter()
            .map(|s| 2 * s.blocks.len())
            .sum()
    }

    #[test]
    fn corrupted_parts_are_rejected() {
        let bytes = encode(&sample_graph(), TINY);
        let mutations: [(&str, &dyn Fn(&mut Vec<u8>)); 5] = [
            ("truncated", &|b| b.truncate(b.len() / 2)),
            ("unterminated varints", &|b| b.fill(0xff)),
            ("large values", &|b| b.fill(0x7f)),
            ("extra byte", &|b| b.push(1)),
            ("first byte bumped", &|b| b[0] = b[0].wrapping_add(1)),
        ];
        let parts = part_count(&bytes);
        let mut rejected = 0;
        for target_part in 0..parts {
            for (name, mutate) in mutations {
                let corrupt = relayout(
                    &bytes,
                    &|n, raw| {
                        if n == target_part {
                            mutate(raw)
                        }
                    },
                    &|_| {},
                );
                match decode(&corrupt) {
                    Err(_) => rejected += 1,
                    Ok(_) => {
                        assert_ne!(name, "unterminated varints", "part {target_part}");
                    }
                }
            }
        }
        // A few mutations only change a value, for example a hash byte, and
        // still decode.
        assert!(
            rejected * 10 > parts * 5 * 9,
            "only {rejected} of {} corruptions rejected",
            parts * 5
        );
    }

    #[rstest]
    #[case::huge_part("rdeps", "claims", &|s: &mut Stream| s.blocks[0].data.uncomp = MAX_PART_BYTES + 1)]
    #[case::inflated_entries("str/target", "the index holds", &|s: &mut Stream| s.blocks[0].entries = 20_000_000)]
    #[case::overlapping_parts("deps", "do not tile", &|s: &mut Stream| s.blocks[0].data.offset = HEADER_BYTES)]
    #[case::frame_size_mismatch("deps", "frame declares", &|s: &mut Stream| s.blocks[0].data.uncomp += 1)]
    #[case::list_mode_missing("ci_srcs", "list mode", &|s: &mut Stream| s.mode = None)]
    fn tampered_directory_is_rejected_before_allocating(
        #[case] stream: &str,
        #[case] message: &str,
        #[case] edit: &dyn Fn(&mut Stream),
    ) {
        let bytes = encode(&sample_graph(), TINY);
        let tampered = relayout(&bytes, &|_, _| {}, &|dir| {
            let s = dir
                .streams
                .iter_mut()
                .find(|s| s.name == stream)
                .expect("stream");
            edit(s);
        });
        let err = decode(&tampered).expect_err("a tampered directory must not load");
        assert!(format!("{err:#}").contains(message), "{err:#}");
    }

    #[test]
    fn a_flipped_byte_inside_a_part_is_rejected() {
        // Incompressible bytes make zstd store a raw block, so only the frame
        // checksum can notice a changed byte.
        let raw: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let (mut comp, uncomp) = compress_part(&[raw]).expect("compress");
        let middle = comp.len() / 2;
        comp[middle] ^= 1;
        let part = Part {
            offset: 0,
            comp: comp.len() as u64,
            uncomp,
        };
        let err = read_part(&file_with(&comp), &part).expect_err("a flipped byte must not load");
        assert!(format!("{err:#}").contains("checksum"), "{err:#}");
    }

    #[test]
    fn a_frame_without_a_checksum_is_rejected() {
        let raw = pack_regions(&[vec![7u8; 64]]);
        let comp = zstd::bulk::compress(&raw, 1).expect("compress");
        let part = Part {
            offset: 0,
            comp: comp.len() as u64,
            uncomp: raw.len() as u64,
        };
        let err =
            read_part(&file_with(&comp), &part).expect_err("an unchecked frame must not load");
        assert!(format!("{err:#}").contains("no checksum"), "{err:#}");
    }

    /// A file with no parts and `raw` as its directory bytes.
    fn file_with_directory(raw: &[u8]) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(SCHEMA_VERSION.to_le_bytes());
        let dir_offset = bytes.len() as u64;
        bytes.extend(compress_frame(raw).expect("compress"));
        bytes.extend(dir_offset.to_le_bytes());
        bytes.extend(MAGIC);
        bytes
    }

    #[test]
    fn a_directory_that_claims_a_huge_name_is_rejected() {
        // Bincode's varints: one stream, then a name of u64::MAX bytes.
        let mut raw = vec![1, 253];
        raw.extend(u64::MAX.to_le_bytes());
        let err = decode(&file_with_directory(&raw)).expect_err("a huge name must not load");
        assert!(
            format!("{err:#}").contains("decoding directory: LimitExceeded"),
            "{err:#}"
        );
    }

    #[test]
    fn bytes_after_the_directory_are_rejected() {
        // An empty stream list, then one extra byte.
        let err = decode(&file_with_directory(&[0, 7])).expect_err("extra bytes must not load");
        assert!(
            format!("{err:#}").contains("1 bytes follow the directory"),
            "{err:#}"
        );
    }

    #[test]
    fn the_directory_checksum_is_verified() {
        let mut bytes = encode(&sample_graph(), TINY);
        // The directory frame ends with its 4-byte checksum, right before the
        // trailer. Changing it leaves the content intact.
        let checksum_byte = bytes.len() - TRAILER_BYTES as usize - 1;
        bytes[checksum_byte] ^= 1;
        let err = decode(&bytes).expect_err("a changed directory must not load");
        let err = format!("{err:#}");
        assert!(
            err.contains("reading directory") && err.contains("checksum"),
            "{err}"
        );
    }

    #[rstest]
    #[case::one_byte(&[0x05], Some(5))]
    #[case::two_bytes(&[0x80, 0x01], Some(128))]
    #[case::max(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01], Some(u64::MAX))]
    #[case::tenth_byte_overflows(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02], None)]
    #[case::too_long(&[0x80; 11], None)]
    #[case::truncated(&[0x80], None)]
    #[case::empty(&[], None)]
    fn varint_decoding(#[case] bytes: &[u8], #[case] expected: Option<u64>) {
        assert_eq!(Cursor::new(bytes).get().ok(), expected);
    }
}
