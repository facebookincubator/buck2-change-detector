/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The changes a graph holds over the snapshot it was read from.
//!
//! A graph read with `read_framed` keeps that snapshot's rows immutable and
//! records every later change per field, by stable key. A delta writes those
//! changes: for each field, the rows stored or changed since the read and the
//! loaded rows removed since. Applying the delta to the same snapshot, read
//! again, gives a graph whose snapshot has the same bytes as one written from
//! the changed graph.
//!
//! Dense snapshots number their ids by sorted strings, so a change can
//! renumber every later id. A delta therefore holds stable keys and values in
//! the runtime value encoding, never dense indices. It is valid only for the
//! exact snapshot it was written against: the writer stores an identity of
//! that snapshot that the caller supplies, and the reader rejects a delta
//! whose identity differs from the one its caller supplies. The caller must
//! make sure that the graph was read from the snapshot that identity names.
//!
//! ```text
//! file  := MAGIC delta_version:u32 schema:u32 identity_len:u32 identity body
//! body  := zstd frame, with checksum and content size, of field*, one per
//!          field in a fixed order
//! field := name_len:varint name rows:varint row*, rows in increasing key order
//! row   := key:u64 0                       (removal of a loaded row)
//!        | key:u64 1 len:varint value      (row stored or changed since the read)
//!        | key:u64 2 n:varint id:u64*n m:varint id:u64*m
//!                                          (unordered list: n IDs removed from the
//!                                           loaded row, then m IDs added)
//! ```
//!
//! A changed edge list of a widely used target can hold millions of IDs, so
//! a list whose order carries no meaning stores a patch of its loaded row.

use std::io::Write;

use anyhow::Context;
use serde::Serialize;
use zstd::zstd_safe::find_frame_compressed_size;

use super::dense::Cursor;
use super::dense::compress_frame;
use super::dense::decompress_frame;
use super::dense::frame_size;
use super::dense::id_of;
use super::dense::put;
use super::packed::Map;
use super::packed::RowChange;
use super::packed::Value;
use super::packed::WORD_BYTES;
use super::*;

const MAGIC: [u8; 4] = *b"TGRD";
/// Version of the layout above. A reader rejects every other version.
const DELTA_VERSION: u32 = 1;
const REMOVED: u8 = 0;
const STORED: u8 = 1;
const PATCHED: u8 = 2;
/// Largest identity a delta can carry. Identities are short digests.
const MAX_IDENTITY_BYTES: u32 = 4096;
/// Largest uncompressed body a writer produces and a reader accepts.
const MAX_BODY_BYTES: u64 = 16 << 30;
/// Magic, delta version, schema and identity length.
const HEADER_BYTES: usize = MAGIC.len() + 3 * size_of::<u32>();
/// The smallest row: a key and a removal.
const MIN_ROW_BYTES: usize = WORD_BYTES + 1;

/// Every field of `TargetGraph`, by the name a delta stores it under. Writing
/// and applying both expand this one list, and `collect` destructures
/// `TargetGraph` from it, so a new field is a compile error until it is listed
/// here. `strings` and `labelsets` mark fields keyed by the hash of their
/// value, which the reader checks; `minimized` rows refer to label sets, which
/// the reader checks against the label sets the graph keeps. `unordered` marks
/// edge lists that snapshots store sorted, whose order carries no meaning;
/// their changed rows can be patches. `set` marks the sudo set, a map to `()`.
macro_rules! delta_fields {
    ($callback:ident) => {
        $callback! {
            ("target", target_id_to_label, strings),
            ("rule", rule_type_id_to_string, strings),
            ("oncall", oncall_id_to_string, strings),
            ("label", label_id_to_string, strings),
            ("minimized", minimized_targets, minimized),
            ("labelsets", label_set_id_to_labels, labelsets),
            ("glob", glob_pattern_id_to_string, strings),
            ("package", package_id_to_path, strings),
            ("file", file_id_to_path, strings),
            ("cidep", ci_deps_pattern_id_to_string, strings),
            ("rdeps", target_id_to_rdeps, unordered),
            ("deps", target_id_to_deps, unordered),
            ("file_rdeps", file_id_to_rdeps, unordered),
            ("file_deps", file_id_to_deps, unordered),
            ("errors", package_id_to_errors, whole),
            ("pkg_targets", package_id_to_targets, unordered),
            ("ci_srcs", target_id_to_ci_srcs, whole),
            ("ci_srcs_mm", target_id_to_ci_srcs_must_match, whole),
            ("cidep_pkg", target_id_to_ci_deps_package_patterns, whole),
            ("cidep_rec", target_id_to_ci_deps_recursive_patterns, whole),
            ("hint_to_affected", ci_hint_to_affected, unordered),
            ("affected_to_hints", affected_to_ci_hints, unordered),
            ("sudo", targets_with_sudo_label, set),
        }
    };
}

/// The rows of one field in a delta.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DeltaFieldSummary {
    pub name: &'static str,
    /// Rows stored or changed since the read.
    pub stored: usize,
    /// Loaded rows removed since the read.
    pub removed: usize,
    /// Changed loaded rows of an unordered list stored as patches.
    pub patched: usize,
    /// Changes that left a loaded row equal to its loaded value, or only
    /// reordered an unordered list. A delta omits them. Only writing counts
    /// them; applying reports zero.
    pub unchanged: usize,
}

impl DeltaFieldSummary {
    /// Rows the delta stores, removes or patches in this field.
    pub fn rows(&self) -> usize {
        self.stored + self.removed + self.patched
    }
}

/// What a delta holds, per field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DeltaSummary {
    pub fields: Vec<DeltaFieldSummary>,
}

impl DeltaSummary {
    /// Rows the delta stores, removes or patches, over all fields.
    pub fn rows(&self) -> usize {
        self.fields.iter().map(DeltaFieldSummary::rows).sum()
    }

    /// Changes that a delta omits because they equal the loaded row.
    pub fn unchanged(&self) -> usize {
        self.fields.iter().map(|f| f.unchanged).sum()
    }
}

/// The changed rows of one field, encoded.
struct FieldRows {
    name: &'static str,
    rows: Vec<(u64, RowChange)>,
    unchanged: usize,
}

impl FieldRows {
    fn of<K: RawId, V: Value>(name: &'static str, field: &Map<K, V>) -> anyhow::Result<Self> {
        Self::from_changes(name, field.is_loaded(), || field.changed_rows())
    }

    fn of_unordered<K: RawId, I: RawId>(
        name: &'static str,
        field: &Map<K, Vec<I>>,
    ) -> anyhow::Result<Self> {
        Self::from_changes(name, field.is_loaded(), || field.changed_unordered_rows())
    }

    fn from_changes(
        name: &'static str,
        loaded: bool,
        changed: impl FnOnce() -> anyhow::Result<(Vec<(u64, RowChange)>, usize)>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            loaded,
            "a delta needs a graph read from a snapshot; {name} was not"
        );
        let (rows, unchanged) =
            changed().with_context(|| format!("collecting the changes of {name}"))?;
        Ok(Self {
            name,
            rows,
            unchanged,
        })
    }

    fn summary(&self) -> DeltaFieldSummary {
        field_summary(
            self.name,
            self.unchanged,
            self.rows.iter().map(|(_, row)| match row {
                RowChange::Removed => RowKind::Removed,
                RowChange::Stored(_) => RowKind::Stored,
                RowChange::Patched { .. } => RowKind::Patched,
            }),
        )
    }
}

/// The kind of a row, as a summary counts it.
enum RowKind {
    Removed,
    Stored,
    Patched,
}

fn field_summary(
    name: &'static str,
    unchanged: usize,
    rows: impl Iterator<Item = RowKind>,
) -> DeltaFieldSummary {
    let mut summary = DeltaFieldSummary {
        name,
        unchanged,
        ..DeltaFieldSummary::default()
    };
    for row in rows {
        match row {
            RowKind::Removed => summary.removed += 1,
            RowKind::Stored => summary.stored += 1,
            RowKind::Patched => summary.patched += 1,
        }
    }
    summary
}

/// The map that backs a field: the sudo set is a map to `()`.
macro_rules! map_of {
    ($field:expr, set) => {
        $field.as_map()
    };
    ($field:expr, $kind:ident) => {
        &$field
    };
}

macro_rules! field_rows {
    ($name:literal, $field:expr, unordered) => {
        FieldRows::of_unordered($name, $field)
    };
    ($name:literal, $field:expr, $kind:ident) => {
        FieldRows::of($name, map_of!(*$field, $kind))
    };
}

fn collect(graph: &TargetGraph) -> anyhow::Result<Vec<FieldRows>> {
    // The pattern names every field of `delta_fields!`, so a new `TargetGraph`
    // field is a compile error here until it is listed. Snapshots do not store
    // the canonicalization counters of a run, so deltas do not either.
    macro_rules! collect_fields {
        ($(($name:literal, $field:ident, $kind:ident)),* $(,)?) => {{
            let TargetGraph { canonicalization_stats: _, $($field,)* } = graph;
            vec![$(field_rows!($name, $field, $kind)?,)*]
        }};
    }
    Ok(delta_fields!(collect_fields))
}

/// Writes the changes `graph` holds over the snapshot identified by
/// `base_identity`, which `graph` must have been read from.
pub(super) fn write(
    graph: &TargetGraph,
    base_identity: &[u8],
    out: &mut dyn Write,
) -> anyhow::Result<DeltaSummary> {
    write_bounded(graph, base_identity, out, MAX_BODY_BYTES)
}

fn write_bounded(
    graph: &TargetGraph,
    base_identity: &[u8],
    out: &mut dyn Write,
    max_body_bytes: u64,
) -> anyhow::Result<DeltaSummary> {
    anyhow::ensure!(
        !base_identity.is_empty(),
        "graph delta base identity is empty"
    );
    let identity_len = u32::try_from(base_identity.len())
        .ok()
        .filter(|&n| n <= MAX_IDENTITY_BYTES)
        .context("graph delta base identity is too long")?;
    let fields = collect(graph)?;
    let body = encode_body(&fields);
    anyhow::ensure!(
        body.len() as u64 <= max_body_bytes,
        "graph delta body of {} bytes exceeds {max_body_bytes}",
        body.len()
    );
    let frame = compress_frame(&body).context("compressing the graph delta")?;
    out.write_all(&MAGIC)
        .and_then(|()| out.write_all(&DELTA_VERSION.to_le_bytes()))
        .and_then(|()| out.write_all(&SCHEMA_VERSION.to_le_bytes()))
        .and_then(|()| out.write_all(&identity_len.to_le_bytes()))
        .and_then(|()| out.write_all(base_identity))
        .and_then(|()| out.write_all(&frame))
        .and_then(|()| out.flush())
        .context("writing the graph delta")?;
    Ok(DeltaSummary {
        fields: fields.iter().map(FieldRows::summary).collect(),
    })
}

fn encode_body(fields: &[FieldRows]) -> Vec<u8> {
    let mut body = Vec::new();
    for field in fields {
        put(&mut body, field.name.len() as u64);
        body.extend_from_slice(field.name.as_bytes());
        put(&mut body, field.rows.len() as u64);
        for (key, row) in &field.rows {
            body.extend_from_slice(&key.to_le_bytes());
            match row {
                RowChange::Removed => body.push(REMOVED),
                RowChange::Stored(bytes) => {
                    body.push(STORED);
                    put(&mut body, bytes.len() as u64);
                    body.extend_from_slice(bytes);
                }
                RowChange::Patched { removed, added } => {
                    body.push(PATCHED);
                    for ids in [removed, added] {
                        put(&mut body, ids.len() as u64);
                        for id in ids {
                            body.extend_from_slice(&id.to_le_bytes());
                        }
                    }
                }
            }
        }
    }
    body
}

/// One validated row: a removal, value bytes borrowed from the body, or a
/// patched list resolved against its loaded row.
enum Parsed<'a, V> {
    Removed,
    Stored(&'a [u8]),
    /// IDs removed from and added to the loaded row, until resolved.
    Patch(Vec<u64>, Vec<u64>),
    Resolved(V),
}

/// The rows of one field, borrowed from a decoded body and validated.
struct ParsedField<'a, V> {
    rows: Vec<(u64, Parsed<'a, V>)>,
}

/// Whether rows of a field can be patches of their loaded rows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Patches {
    Allowed,
    Rejected,
}

/// The label-set rows that a delta removes or stores and the label sets that
/// its stored targets refer to, checked together before any row changes: the
/// graph must hold every label set that a remaining target refers to.
#[derive(Default)]
struct LabelSetRefs {
    removed: IdHashSet<u64>,
    stored: IdHashSet<u64>,
    /// Targets whose minimized row the delta removes or stores.
    targets: IdHashSet<u64>,
    /// Each stored minimized target and its label set.
    referenced: Vec<(u64, u64)>,
}

impl LabelSetRefs {
    fn check(&self, graph: &TargetGraph) -> anyhow::Result<()> {
        let kept = |set: u64| {
            self.stored.contains(&set)
                || (!self.removed.contains(&set)
                    && graph
                        .label_set_id_to_labels
                        .loaded_position(&LabelSetId::from_raw(set))
                        .is_some())
        };
        for &(target, set) in &self.referenced {
            anyhow::ensure!(
                kept(set),
                "minimized: row {target:x} refers to label set {set:x}, which the graph would not hold"
            );
        }
        if self.removed.is_empty() {
            return Ok(());
        }
        let dangling = graph.minimized_targets.par_iter().find_map_any(|row| {
            let target = row.key().raw();
            let set = row.value().label_set.raw();
            (!self.targets.contains(&target) && !kept(set)).then_some((target, set))
        });
        match dangling {
            Some((target, set)) => anyhow::bail!(
                "labelsets: the delta removes label set {set:x}, which target {target:x} still refers to"
            ),
            None => Ok(()),
        }
    }
}

fn ids(c: &mut Cursor<'_>) -> anyhow::Result<Vec<u64>> {
    let n = c.index()?;
    anyhow::ensure!(n <= c.remaining() / WORD_BYTES, "truncated patch");
    let mut ids = Vec::new();
    ids.try_reserve_exact(n)
        .with_context(|| format!("reserving {n} patch IDs"))?;
    for _ in 0..n {
        ids.push(u64::from_le_bytes(c.array()?));
    }
    Ok(ids)
}

/// Reads the header, checks that the delta was written by this layout and
/// schema against `base_identity`, and returns the decompressed body.
fn read_body(delta: &[u8], base_identity: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        !base_identity.is_empty(),
        "graph delta base identity is empty"
    );
    anyhow::ensure!(delta.len() >= HEADER_BYTES, "graph delta too small");
    let mut c = Cursor::new(delta);
    let magic: [u8; 4] = c.array()?;
    anyhow::ensure!(magic == MAGIC, "not a graph delta: magic {magic:?}");
    let version = u32::from_le_bytes(c.array()?);
    anyhow::ensure!(
        version == DELTA_VERSION,
        "graph delta version mismatch: expected {DELTA_VERSION}, got {version}"
    );
    let schema = u32::from_le_bytes(c.array()?);
    anyhow::ensure!(
        schema == SCHEMA_VERSION,
        "graph delta schema mismatch: expected {SCHEMA_VERSION}, got {schema}"
    );
    let identity_len = u32::from_le_bytes(c.array()?);
    anyhow::ensure!(
        identity_len <= MAX_IDENTITY_BYTES,
        "graph delta base identity of {identity_len} bytes is too long"
    );
    let identity = c
        .bytes(identity_len as usize)
        .context("truncated graph delta base identity")?;
    anyhow::ensure!(
        identity == base_identity,
        "graph delta was written for another base: delta {:?}, base {:?}",
        String::from_utf8_lossy(identity),
        String::from_utf8_lossy(base_identity)
    );
    let frame = c.bytes(c.remaining()).expect("remaining bytes");
    let frame_len = find_frame_compressed_size(frame)
        .map_err(|code| anyhow::anyhow!("reading the graph delta body: zstd error {code}"))?;
    anyhow::ensure!(
        frame_len == frame.len(),
        "{} bytes follow the graph delta body",
        frame.len().saturating_sub(frame_len)
    );
    let size = frame_size(frame).context("reading the graph delta body")?;
    anyhow::ensure!(
        size <= MAX_BODY_BYTES,
        "graph delta body of {size} bytes exceeds {MAX_BODY_BYTES}"
    );
    decompress_frame(frame, size).context("reading the graph delta body")
}

fn parse_field<'a, K: RawId, V: Value>(
    c: &mut Cursor<'a>,
    name: &'static str,
    field: &Map<K, V>,
    patches: Patches,
) -> anyhow::Result<ParsedField<'a, V>> {
    let header = || format!("{name}: truncated field header");
    let name_len = c.index().with_context(header)?;
    let stored_name = c.bytes(name_len).with_context(header)?;
    anyhow::ensure!(
        stored_name == name.as_bytes(),
        "graph delta field {:?} found where {name} belongs",
        String::from_utf8_lossy(stored_name)
    );
    let n = c.index().with_context(header)?;
    // The body bounds the allocation.
    anyhow::ensure!(n <= c.remaining() / MIN_ROW_BYTES, "{name}: truncated rows");
    let mut rows = Vec::new();
    rows.try_reserve_exact(n)
        .with_context(|| format!("{name}: reserving {n} rows"))?;
    let mut previous = None;
    for _ in 0..n {
        let key = u64::from_le_bytes(
            c.array()
                .with_context(|| format!("{name}: truncated row"))?,
        );
        anyhow::ensure!(
            previous.is_none_or(|p| p < key),
            "{name}: keys are not strictly increasing"
        );
        previous = Some(key);
        let value =
            parse_row(c, field, key, patches).with_context(|| format!("{name}: row {key:x}"))?;
        rows.push((key, value));
    }
    Ok(ParsedField { rows })
}

fn parse_row<'a, K: RawId, V: Value>(
    c: &mut Cursor<'a>,
    field: &Map<K, V>,
    key: u64,
    patches: Patches,
) -> anyhow::Result<Parsed<'a, V>> {
    Ok(match c.bytes(1)?[0] {
        REMOVED => {
            anyhow::ensure!(
                field.loaded_position(&K::from_raw(key)).is_some(),
                "the delta removes this row, which its base does not hold"
            );
            Parsed::Removed
        }
        STORED => {
            let len = c.index()?;
            let bytes = c.bytes(len)?;
            V::validate(bytes)?;
            Parsed::Stored(bytes)
        }
        PATCHED => {
            anyhow::ensure!(
                patches == Patches::Allowed,
                "rows of this field cannot be patches"
            );
            let removed = ids(c)?;
            let added = ids(c)?;
            Parsed::Patch(removed, added)
        }
        kind => anyhow::bail!("unknown row kind {kind}"),
    })
}

/// Resolves each patch of an unordered list against its loaded row. Resolving
/// reads the whole loaded row, so rows resolve in parallel, before any row
/// changes.
fn resolve_patches<K: RawId, I: RawId>(
    name: &'static str,
    field: &Map<K, Vec<I>>,
    parsed: &mut ParsedField<'_, Vec<I>>,
) -> anyhow::Result<()> {
    parsed.rows.par_iter_mut().try_for_each(|(key, row)| {
        if let Parsed::Patch(removed, added) = row {
            let value = field
                .patched_loaded_row(K::from_raw(*key), removed, added)
                .with_context(|| format!("{name}: patch of row {key:x}"))?;
            *row = Parsed::Resolved(value);
        }
        anyhow::Ok(())
    })
}

/// Checks that each stored string row is keyed by the hash of its string, as
/// a snapshot reader derives it.
fn check_string_keys<K: RawId>(
    name: &'static str,
    _field: &Map<K, String>,
    parsed: &ParsedField<'_, String>,
) -> anyhow::Result<()> {
    for (key, row) in &parsed.rows {
        if let Parsed::Stored(bytes) = row {
            let text = str::from_utf8(bytes).expect("validated string");
            anyhow::ensure!(
                id_of::<K>(text).raw() == *key,
                "{name}: row {key:x} is not keyed by the hash of its string"
            );
        }
    }
    Ok(())
}

/// Checks that each stored label set is keyed by the hash of its labels, and
/// records the label sets that the delta removes or stores.
fn check_label_set_keys(
    name: &'static str,
    parsed: &ParsedField<'_, Vec<LabelId>>,
    refs: &mut LabelSetRefs,
) -> anyhow::Result<()> {
    for (key, row) in &parsed.rows {
        match row {
            Parsed::Stored(bytes) => {
                anyhow::ensure!(
                    LabelSetId::of(&Vec::<LabelId>::decode(bytes)).raw() == *key,
                    "{name}: row {key:x} is not keyed by the hash of its labels"
                );
                refs.stored.insert(*key);
            }
            Parsed::Removed => {
                refs.removed.insert(*key);
            }
            Parsed::Patch(..) | Parsed::Resolved(_) => unreachable!("label sets cannot be patches"),
        }
    }
    Ok(())
}

/// Records the targets whose minimized row the delta removes or stores, and
/// the label sets that the stored rows refer to.
fn record_label_set_refs(parsed: &ParsedField<'_, StoredMinimizedTarget>, refs: &mut LabelSetRefs) {
    for (key, row) in &parsed.rows {
        refs.targets.insert(*key);
        if let Parsed::Stored(bytes) = row {
            refs.referenced
                .push((*key, StoredMinimizedTarget::decode(bytes).label_set.raw()));
        }
    }
}

fn apply_field<K: RawId, V: Value>(field: &Map<K, V>, parsed: ParsedField<'_, V>) {
    parsed.rows.into_par_iter().for_each(|(key, value)| {
        let key = K::from_raw(key);
        match value {
            Parsed::Stored(bytes) => {
                field.insert(key, V::decode(bytes));
            }
            Parsed::Resolved(value) => {
                field.insert(key, value);
            }
            Parsed::Removed => {
                field.discard(&key);
            }
            Parsed::Patch(..) => unreachable!("patches are resolved while parsing"),
        }
    });
}

fn summary_of<V>(name: &'static str, parsed: &ParsedField<'_, V>) -> DeltaFieldSummary {
    field_summary(
        name,
        0,
        parsed.rows.iter().map(|(_, row)| match row {
            Parsed::Removed => RowKind::Removed,
            Parsed::Stored(_) => RowKind::Stored,
            Parsed::Patch(..) | Parsed::Resolved(_) => RowKind::Patched,
        }),
    )
}

macro_rules! patches {
    (unordered) => {
        Patches::Allowed
    };
    ($kind:ident) => {
        Patches::Rejected
    };
}

/// Checks or resolves the parsed rows of a field as its kind requires.
macro_rules! finish_field {
    ($name:literal, $field:expr, $parsed:expr, $refs:expr, strings) => {{
        let parsed = $parsed;
        check_string_keys($name, $field, &parsed).map(|()| parsed)
    }};
    ($name:literal, $field:expr, $parsed:expr, $refs:expr, labelsets) => {{
        let parsed = $parsed;
        check_label_set_keys($name, &parsed, $refs).map(|()| parsed)
    }};
    ($name:literal, $field:expr, $parsed:expr, $refs:expr, minimized) => {{
        let parsed = $parsed;
        record_label_set_refs(&parsed, $refs);
        anyhow::Ok(parsed)
    }};
    ($name:literal, $field:expr, $parsed:expr, $refs:expr, unordered) => {{
        let mut parsed = $parsed;
        resolve_patches($name, $field, &mut parsed).map(|()| parsed)
    }};
    ($name:literal, $field:expr, $parsed:expr, $refs:expr, $kind:ident) => {
        anyhow::Ok($parsed)
    };
}

/// Applies a delta that `write` wrote against the snapshot identified by
/// `base_identity` to `graph`, which must have been read from that snapshot
/// and not changed since. The whole delta is validated before any row
/// changes, so an error leaves `graph` unchanged.
pub(super) fn apply(
    graph: &TargetGraph,
    delta: &[u8],
    base_identity: &[u8],
) -> anyhow::Result<DeltaSummary> {
    let body = read_body(delta, base_identity)?;
    let mut c = Cursor::new(&body);
    let mut summaries = Vec::new();
    let mut refs = LabelSetRefs::default();
    macro_rules! parse_fields {
        ($(($name:literal, $field:ident, $kind:ident)),* $(,)?) => {
            ($({
                let field = map_of!(graph.$field, $kind);
                anyhow::ensure!(
                    field.is_loaded() && field.is_unchanged(),
                    "a graph delta applies only to an unchanged graph read from its base; {} is not",
                    $name
                );
                let parsed = finish_field!(
                    $name,
                    field,
                    parse_field(&mut c, $name, field, patches!($kind))?,
                    &mut refs,
                    $kind
                )?;
                summaries.push(summary_of($name, &parsed));
                parsed
            },)*)
        };
    }
    let parsed_fields = delta_fields!(parse_fields);
    anyhow::ensure!(c.done(), "{} bytes follow the graph delta", c.remaining());
    refs.check(graph)?;

    macro_rules! apply_fields {
        ($(($name:literal, $field:ident, $kind:ident)),* $(,)?) => {{
            let ($($field,)*) = parsed_fields;
            rayon::scope(|s| {
                $(s.spawn(move |_| apply_field(map_of!(graph.$field, $kind), $field));)*
            });
        }};
    }
    delta_fields!(apply_fields);
    Ok(DeltaSummary { fields: summaries })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use zstd::zstd_safe::MAGIC_SKIPPABLE_START;
    use zstd::zstd_safe::MAGICNUMBER;

    use super::super::dense::list_is_sorted;
    use super::super::dense::tests::decode;
    use super::super::dense::tests::sample_graph;
    use super::super::dense::tests::snapshot;
    use super::super::dense::tests::target;
    use super::*;

    const IDENTITY: &[u8] = b"schema=15;bytes=1;blake3=00";

    fn snapshot_bytes(graph: &TargetGraph) -> Vec<u8> {
        let mut bytes = Vec::new();
        graph.write_framed(&mut bytes).expect("write");
        bytes
    }

    fn read(bytes: &[u8]) -> TargetGraph {
        decode(bytes).expect("read")
    }

    /// Changes every field of a loaded graph: new and removed targets with new
    /// strings, moved edges with rdeps, labels, ci_srcs, package errors, sudo,
    /// file imports and ci_hint edges.
    fn mutate(graph: &TargetGraph) {
        let new = graph.store_target("fbcode//0:new ✓");
        let rule = graph.store_rule_type("0:new_rule");
        let oncall = graph.store_oncall("0_new_oncall");
        let label = graph.store_label("0:new_label");
        graph.store_minimized_target(
            new,
            MinimizedBuckTarget {
                rule_type: rule,
                oncall: Some(oncall),
                labels: vec![label, graph.store_label("ci:land")],
                target_hash: TargetHash::new(&format!("{:032x}", 99)),
            },
        );
        graph.add_target_to_package(graph.store_package("fbcode//0"), new);
        // Move t01's edge from the hub to the new target.
        let edited = target("fbcode//a:t01");
        let removals = IdDashMap::default();
        for old in graph.get_deps(edited).unwrap_or_default() {
            removals.entry(old).or_insert_with(Vec::new).push(edited);
        }
        let additions = IdDashMap::default();
        additions.insert(new, vec![edited]);
        graph.batch_update_rdeps(additions, removals);
        graph.set_deps(edited, vec![new]);
        // Remove a target that has deps, rdeps, ci_srcs and a ci_hint edge.
        graph.remove_targets_batch(&[target("fbcode//a:t03")].into_iter().collect());
        // Labels, ci_srcs, ci_deps patterns, errors and sudo.
        let relabelled = target("fbcode//a:t04");
        let mut minimized = graph.get_minimized_target(relabelled).expect("target");
        minimized.labels = vec![label];
        graph.store_minimized_target(relabelled, minimized);
        graph.store_ci_srcs(target("fbcode//a:t01"), vec![]);
        graph.add_ci_src(relabelled, graph.store_glob_pattern("0/new/**"));
        graph.add_ci_src(relabelled, graph.store_glob_pattern("include/**"));
        graph.add_ci_src_must_match(relabelled, graph.store_glob_pattern("0/new/**"));
        graph.add_ci_deps_package_pattern(relabelled, graph.store_ci_deps_pattern("fbcode//0"));
        graph.add_ci_deps_recursive_pattern(relabelled, graph.store_ci_deps_pattern("fbcode//b"));
        graph.clear_errors(graph.store_package("fbcode//é"));
        graph.add_error(graph.store_package("fbcode//0"), "new error ✓".to_owned());
        graph.unmark_target_has_sudo_label(target("fbcode//a:t02"));
        graph.mark_target_has_sudo_label(relabelled);
        // A new ci_hint edge and a new file import.
        let hint = target("fbcode//a:hint");
        graph.add_ci_hint_edge(hint, relabelled);
        let defs = graph.store_file("fbcode/0/new.bzl");
        graph.add_file_import(defs, graph.store_file("fbcode/a/defs0.bzl"));
        // A no-op override: read-modify-write that restores the loaded value.
        let untouched = target("fbcode//a:t10");
        let rdeps = graph.get_rdeps(untouched).expect("rdeps");
        graph.target_id_to_rdeps.insert(untouched, rdeps);
    }

    #[test]
    fn applying_a_delta_to_its_base_writes_the_changed_snapshot() -> anyhow::Result<()> {
        let base_bytes = snapshot_bytes(&sample_graph());
        let changed = read(&base_bytes);
        mutate(&changed);
        let mut delta = Vec::new();
        let written = changed.write_delta(IDENTITY, &mut delta)?;
        assert!(written.rows() > 0);
        assert_eq!(written.unchanged(), 1, "{written:?}");

        let rebuilt = read(&base_bytes);
        let applied = rebuilt.apply_delta(&delta, IDENTITY)?;
        assert_eq!(applied.rows(), written.rows());
        assert_eq!(snapshot(&rebuilt), snapshot(&changed));
        let expected = snapshot_bytes(&changed);
        assert_eq!(snapshot_bytes(&rebuilt), expected);
        // The rebuilt graph's changes are a delta of the same graph. Patches
        // need not repeat: an applied patch appends its added IDs.
        let mut again = Vec::new();
        rebuilt.write_delta(IDENTITY, &mut again)?;
        let twice = read(&base_bytes);
        twice.apply_delta(&again, IDENTITY)?;
        assert_eq!(snapshot_bytes(&twice), expected);
        Ok(())
    }

    /// A hub target with many dependents, loaded from a snapshot.
    fn hub_graph(dependents: usize) -> (Vec<u8>, TargetId, Vec<TargetId>) {
        let graph = TargetGraph::new();
        let hub = graph.store_target("fbcode//hub:hub");
        let users: Vec<TargetId> = (0..dependents)
            .map(|i| graph.store_target(&format!("fbcode//u:{i}")))
            .collect();
        for &u in &users {
            graph.store_deps_with_rdeps(u, vec![hub]);
        }
        (snapshot_bytes(&graph), hub, users)
    }

    fn round_trip(
        base_bytes: &[u8],
        changed: &TargetGraph,
    ) -> anyhow::Result<(Vec<u8>, DeltaSummary)> {
        let mut delta = Vec::new();
        let summary = changed.write_delta(IDENTITY, &mut delta)?;
        let rebuilt = read(base_bytes);
        rebuilt.apply_delta(&delta, IDENTITY)?;
        assert_eq!(snapshot(&rebuilt), snapshot(changed));
        assert_eq!(snapshot_bytes(&rebuilt), snapshot_bytes(changed));
        Ok((delta, summary))
    }

    fn rdeps_summary(summary: &DeltaSummary) -> &DeltaFieldSummary {
        summary.fields.iter().find(|f| f.name == "rdeps").unwrap()
    }

    #[test]
    fn hub_edges_are_patches_of_their_loaded_rows() -> anyhow::Result<()> {
        let (base_bytes, hub, users) = hub_graph(20_000);
        let row_bytes = 20_000 * 8;
        // Append, swap removal and order-keeping removal of hub edges.
        let appended = read(&base_bytes);
        let new = appended.store_target("fbcode//u:new");
        appended.store_deps_with_rdeps(new, vec![hub]);
        let swapped = read(&base_bytes);
        swapped.remove_from_rdeps(hub, users[3]);
        swapped.set_deps(users[3], vec![]);
        let retained = read(&base_bytes);
        retained.remove_targets_batch(&[users[5], users[7]].into_iter().collect());
        // A swap removal and an order-keeping removal on the same row
        // defeat both linear pairings.
        let mixed = read(&base_bytes);
        mixed.remove_from_rdeps(hub, users[3]);
        mixed.set_deps(users[3], vec![]);
        mixed.remove_targets_batch(&[users[5], users[7]].into_iter().collect());
        mixed.store_deps_with_rdeps(mixed.store_target("fbcode//u:new"), vec![hub]);
        for changed in [&appended, &swapped, &retained, &mixed] {
            let (delta, summary) = round_trip(&base_bytes, changed)?;
            assert_eq!(rdeps_summary(&summary).patched, 1, "{summary:?}");
            assert!(delta.len() < row_bytes / 20, "{} delta bytes", delta.len());
        }
        Ok(())
    }

    #[test]
    fn reordering_an_unordered_row_is_unchanged() -> anyhow::Result<()> {
        let (base_bytes, hub, _) = hub_graph(10);
        let graph = read(&base_bytes);
        let mut rdeps = graph.get_rdeps(hub).unwrap();
        rdeps.reverse();
        graph.target_id_to_rdeps.insert(hub, rdeps);
        let (_, summary) = round_trip(&base_bytes, &graph)?;
        assert_eq!(summary.rows(), 0);
        assert_eq!(rdeps_summary(&summary).unchanged, 1);
        Ok(())
    }

    #[test]
    fn a_patch_that_removes_a_missing_id_is_rejected() -> anyhow::Result<()> {
        let (base_bytes, hub, users) = hub_graph(100);
        let changed = read(&base_bytes);
        changed.remove_from_rdeps(hub, users[0]);
        let mut delta = Vec::new();
        changed.write_delta(IDENTITY, &mut delta)?;
        // A base without that dependent has the same identity claim but
        // cannot take the patch.
        let other = TargetGraph::new();
        let other_hub = other.store_target("fbcode//hub:hub");
        for (i, &u) in users.iter().enumerate().skip(1) {
            other.store_target(&format!("fbcode//u:{i}"));
            other.store_deps_with_rdeps(u, vec![other_hub]);
        }
        let other = read(&snapshot_bytes(&other));
        let error = other.apply_delta(&delta, IDENTITY).unwrap_err();
        assert!(
            format!("{error:#}").contains("a patch removes IDs that the loaded row does not hold"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn every_field_appears_in_a_delta() -> anyhow::Result<()> {
        let changed = read(&snapshot_bytes(&sample_graph()));
        mutate(&changed);
        let summary = changed.write_delta(IDENTITY, &mut Vec::new())?;
        let untouched: Vec<_> = summary
            .fields
            .iter()
            .filter(|f| f.rows() == 0)
            .map(|f| f.name)
            .collect();
        assert_eq!(untouched, Vec::<&str>::new(), "{summary:?}");
        Ok(())
    }

    #[test]
    fn an_unchanged_graph_writes_an_empty_delta() -> anyhow::Result<()> {
        let base_bytes = snapshot_bytes(&sample_graph());
        let graph = read(&base_bytes);
        let mut delta = Vec::new();
        assert_eq!(graph.write_delta(IDENTITY, &mut delta)?.rows(), 0);
        let rebuilt = read(&base_bytes);
        rebuilt.apply_delta(&delta, IDENTITY)?;
        assert_eq!(snapshot_bytes(&rebuilt), base_bytes);
        Ok(())
    }

    #[test]
    fn a_fresh_graph_has_no_base_for_a_delta() {
        let error = sample_graph()
            .write_delta(IDENTITY, &mut Vec::new())
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("read from a snapshot"),
            "{error:#}"
        );
    }

    fn written_delta() -> (Vec<u8>, Vec<u8>) {
        let base_bytes = snapshot_bytes(&sample_graph());
        let changed = read(&base_bytes);
        mutate(&changed);
        let mut delta = Vec::new();
        changed.write_delta(IDENTITY, &mut delta).expect("delta");
        (base_bytes, delta)
    }

    #[test]
    fn another_base_identity_is_rejected() {
        let (base_bytes, delta) = written_delta();
        let rebuilt = read(&base_bytes);
        let error = rebuilt
            .apply_delta(&delta, b"schema=15;bytes=2;blake3=00")
            .unwrap_err();
        assert!(format!("{error:#}").contains("another base"), "{error:#}");
        assert_eq!(snapshot_bytes(&rebuilt), base_bytes);
    }

    #[test]
    fn a_base_without_a_removed_row_is_rejected() {
        let (_, delta) = written_delta();
        // Another base with the same identity claim: the removed target and
        // the label of the removed sudo target are missing.
        let other = sample_graph();
        other.remove_target(target("fbcode//a:t03"));
        other.unmark_target_has_sudo_label(target("fbcode//a:t02"));
        let other = read(&snapshot_bytes(&other));
        let error = other.apply_delta(&delta, IDENTITY).unwrap_err();
        assert!(
            format!("{error:#}").contains("which its base does not hold"),
            "{error:#}"
        );
    }

    #[test]
    fn a_changed_graph_cannot_take_a_delta() {
        let (base_bytes, delta) = written_delta();
        let rebuilt = read(&base_bytes);
        rebuilt.store_target("fbcode//z:z");
        let error = rebuilt.apply_delta(&delta, IDENTITY).unwrap_err();
        assert!(
            format!("{error:#}").contains("unchanged graph"),
            "{error:#}"
        );
    }

    /// Header offsets: magic, then the delta version, schema and identity
    /// length, each a u32.
    const VERSION_AT: usize = MAGIC.len();
    const SCHEMA_AT: usize = VERSION_AT + size_of::<u32>();
    const IDENTITY_LEN_AT: usize = SCHEMA_AT + size_of::<u32>();

    #[rstest]
    #[case::magic(0, "not a graph delta")]
    #[case::version(VERSION_AT, "delta version mismatch")]
    #[case::schema(SCHEMA_AT, "delta schema mismatch")]
    fn a_damaged_header_is_rejected(#[case] offset: usize, #[case] message: &str) {
        let (base_bytes, mut delta) = written_delta();
        delta[offset] ^= 0xff;
        let error = read(&base_bytes).apply_delta(&delta, IDENTITY).unwrap_err();
        assert!(format!("{error:#}").contains(message), "{error:#}");
    }

    #[test]
    fn a_flipped_body_byte_or_truncation_is_rejected() {
        let (base_bytes, delta) = written_delta();
        let header = HEADER_BYTES + IDENTITY.len();
        for damaged in [
            {
                let mut d = delta.clone();
                let last = d.len() - 6;
                d[last] ^= 0x01;
                d
            },
            delta[..delta.len() - 1].to_vec(),
            delta[..header].to_vec(),
        ] {
            let rebuilt = read(&base_bytes);
            assert!(rebuilt.apply_delta(&damaged, IDENTITY).is_err());
            assert_eq!(snapshot_bytes(&rebuilt), base_bytes);
        }
        let mut trailing = delta.clone();
        trailing.push(0);
        assert!(read(&base_bytes).apply_delta(&trailing, IDENTITY).is_err());
    }

    /// FNV-1a, a stable digest for pinning bytes in a test.
    fn fnv1a(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf29ce484222325, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
    }

    /// A change of a small graph that writes every row kind and every value
    /// encoding: strings, minimized targets with a hex hash and an oncall and
    /// with a text hash and none, label sets, errors, glob lists, a stored and
    /// a patched unordered list, the sudo set, and removals.
    fn pinned_change() -> TargetGraph {
        let base = TargetGraph::new();
        let hub = base.store_target("fbcode//hub:hub");
        let users: Vec<TargetId> = (0..8)
            .map(|i| base.store_target(&format!("fbcode//u:{i}")))
            .collect();
        for &user in &users {
            base.store_deps_with_rdeps(user, vec![hub]);
        }
        let rule = base.store_rule_type("rule");
        base.store_minimized_target(
            users[0],
            MinimizedBuckTarget {
                rule_type: rule,
                oncall: Some(base.store_oncall("oncall")),
                labels: vec![base.store_label("a")],
                target_hash: TargetHash::new(&format!("{:032x}", 1)),
            },
        );
        let package = base.store_package("fbcode//p");
        base.add_error(package, "old".to_owned());
        base.mark_target_has_sudo_label(users[1]);

        let changed = read(&snapshot_bytes(&base));
        let new = changed.store_target("fbcode//n:n");
        changed.store_minimized_target(
            new,
            MinimizedBuckTarget {
                rule_type: rule,
                oncall: None,
                labels: vec![changed.store_label("b")],
                target_hash: TargetHash::new("not hex"),
            },
        );
        changed.store_deps_with_rdeps(new, vec![hub]);
        changed.add_ci_src(new, changed.store_glob_pattern("n/**"));
        changed.clear_errors(package);
        changed.add_error(package, "new".to_owned());
        changed.store_minimized_target(
            users[0],
            MinimizedBuckTarget {
                rule_type: rule,
                oncall: Some(changed.store_oncall("oncall 2")),
                labels: vec![changed.store_label("a")],
                target_hash: TargetHash::new(&format!("{:032x}", 2)),
            },
        );
        changed.unmark_target_has_sudo_label(users[1]);
        changed.mark_target_has_sudo_label(users[2]);
        changed.remove_targets_batch(&[users[7]].into_iter().collect());
        changed
    }

    #[test]
    fn the_body_encoding_is_pinned_to_the_delta_version() -> anyhow::Result<()> {
        let fields = collect(&pinned_change())?;
        let patched = fields
            .iter()
            .flat_map(|f| &f.rows)
            .filter(|(_, row)| matches!(row, RowChange::Patched { .. }))
            .count();
        assert!(patched > 0, "the fixture writes a patch");
        let body = encode_body(&fields);
        // A change to the runtime value encoding or to the body layout changes
        // this digest: give it a new DELTA_VERSION, then update the digest.
        assert_eq!(
            (DELTA_VERSION, body.len(), fnv1a(&body)),
            (1, 519, 0xd44c_e3a8_3ead_31e9),
            "the graph delta body encoding changed"
        );
        Ok(())
    }

    #[test]
    fn a_body_over_the_limit_is_not_written() {
        let changed = read(&snapshot_bytes(&sample_graph()));
        mutate(&changed);
        let mut out = Vec::new();
        let error = write_bounded(&changed, IDENTITY, &mut out, 16).unwrap_err();
        assert!(format!("{error:#}").contains("exceeds 16"), "{error:#}");
        assert!(out.is_empty());
    }

    #[test]
    fn a_frame_after_the_body_is_rejected() {
        let (base_bytes, mut delta) = written_delta();
        // A zstd skippable frame with an empty payload.
        delta.extend_from_slice(&MAGIC_SKIPPABLE_START.to_le_bytes());
        delta.extend_from_slice(&0u32.to_le_bytes());
        let error = read(&base_bytes).apply_delta(&delta, IDENTITY).unwrap_err();
        assert!(
            format!("{error:#}").contains("follow the graph delta body"),
            "{error:#}"
        );
    }

    /// A row stored under a key that is not the hash of its value.
    enum BadKey {
        String,
        LabelSet,
    }

    #[rstest]
    #[case::string(BadKey::String, "is not keyed by the hash of its string")]
    #[case::label_set(BadKey::LabelSet, "is not keyed by the hash of its labels")]
    fn a_row_not_keyed_by_its_hash_is_rejected(#[case] bad: BadKey, #[case] message: &str) {
        let base_bytes = snapshot_bytes(&sample_graph());
        let changed = read(&base_bytes);
        match bad {
            BadKey::String => changed
                .target_id_to_label
                .insert(TargetId::from_raw(0x1234), "fbcode//bad:key".to_owned()),
            BadKey::LabelSet => {
                let label = changed.store_label("bad_key");
                changed
                    .label_set_id_to_labels
                    .insert(LabelSetId::from_raw(0x1234), vec![label])
            }
        };
        let mut delta = Vec::new();
        changed.write_delta(IDENTITY, &mut delta).expect("delta");
        let rebuilt = read(&base_bytes);
        let error = rebuilt.apply_delta(&delta, IDENTITY).unwrap_err();
        assert!(format!("{error:#}").contains(message), "{error:#}");
        assert_eq!(snapshot_bytes(&rebuilt), base_bytes);
    }

    /// A change that leaves a target referring to a missing label set.
    enum Dangling {
        RemovedSet,
        MissingSet,
    }

    #[rstest]
    #[case::removed_label_set_still_referenced(Dangling::RemovedSet, "still refers to")]
    #[case::stored_target_with_a_missing_label_set(Dangling::MissingSet, "would not hold")]
    fn a_dangling_label_set_reference_is_rejected(
        #[case] dangling: Dangling,
        #[case] message: &str,
    ) {
        let base_bytes = snapshot_bytes(&sample_graph());
        let changed = read(&base_bytes);
        let relabelled = target("fbcode//a:t04");
        let label_set = changed
            .minimized_targets
            .get(&relabelled)
            .expect("target")
            .value()
            .label_set;
        match dangling {
            Dangling::RemovedSet => {
                changed.label_set_id_to_labels.discard(&label_set);
            }
            Dangling::MissingSet => {
                let mut stored = changed
                    .minimized_targets
                    .get(&relabelled)
                    .expect("target")
                    .to_owned_value();
                stored.label_set = LabelSetId::from_raw(0x1234);
                changed.minimized_targets.insert(relabelled, stored);
            }
        }
        let mut delta = Vec::new();
        changed.write_delta(IDENTITY, &mut delta).expect("delta");
        let rebuilt = read(&base_bytes);
        let error = rebuilt.apply_delta(&delta, IDENTITY).unwrap_err();
        assert!(format!("{error:#}").contains(message), "{error:#}");
        assert_eq!(snapshot_bytes(&rebuilt), base_bytes);
    }

    /// A delta for `IDENTITY` around a hand-written body.
    fn delta_with_body(body: &[u8]) -> Vec<u8> {
        let mut delta = MAGIC.to_vec();
        delta.extend_from_slice(&DELTA_VERSION.to_le_bytes());
        delta.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
        delta.extend_from_slice(&(IDENTITY.len() as u32).to_le_bytes());
        delta.extend_from_slice(IDENTITY);
        delta.extend_from_slice(&compress_frame(body).expect("compress"));
        delta
    }

    /// A body in which every field is empty except `field`, which holds
    /// `rows` rows written as `row_bytes`.
    fn body_with(graph: &TargetGraph, field: &str, rows: u64, row_bytes: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        for name in collect(graph).expect("fields").iter().map(|f| f.name) {
            put(&mut body, name.len() as u64);
            body.extend_from_slice(name.as_bytes());
            if name == field {
                put(&mut body, rows);
                body.extend_from_slice(row_bytes);
            } else {
                put(&mut body, 0);
            }
        }
        body
    }

    /// A stored target row: its key, then the value bytes.
    fn stored_target(key: u64, value: &[u8]) -> Vec<u8> {
        let mut row = key.to_le_bytes().to_vec();
        row.push(STORED);
        put(&mut row, value.len() as u64);
        row.extend_from_slice(value);
        row
    }

    #[test]
    fn a_field_in_the_wrong_place_is_rejected() {
        let base_bytes = snapshot_bytes(&sample_graph());
        let rebuilt = read(&base_bytes);
        let mut body = Vec::new();
        put(&mut body, 5);
        body.extend_from_slice(b"wrong");
        put(&mut body, 0);
        let error = rebuilt
            .apply_delta(&delta_with_body(&body), IDENTITY)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("graph delta field \"wrong\" found where target belongs"),
            "{error:#}"
        );
    }

    #[test]
    fn an_oversized_identity_is_rejected() {
        let base_bytes = snapshot_bytes(&sample_graph());
        let mut delta = delta_with_body(&[]);
        delta[IDENTITY_LEN_AT..HEADER_BYTES]
            .copy_from_slice(&(MAX_IDENTITY_BYTES + 1).to_le_bytes());
        let error = read(&base_bytes).apply_delta(&delta, IDENTITY).unwrap_err();
        assert!(format!("{error:#}").contains("is too long"), "{error:#}");
    }

    #[test]
    fn unordered_fields_are_the_lists_snapshots_store_sorted() {
        macro_rules! kinds {
            ($(($name:literal, $field:ident, $kind:ident)),* $(,)?) => {
                vec![$(($name, stringify!($kind))),*]
            };
        }
        for (name, kind) in delta_fields!(kinds) {
            match list_is_sorted(name) {
                Some(sorted) => assert_eq!(sorted, kind == "unordered", "{name}"),
                None => assert_ne!(kind, "unordered", "{name}"),
            }
        }
    }

    #[rstest]
    #[case::unknown_row_kind(
        "target",
        1,
        [7u64.to_le_bytes().as_slice(), &[9]].concat(),
        "unknown row kind 9"
    )]
    #[case::patch_of_a_whole_field(
        "target",
        1,
        [7u64.to_le_bytes().as_slice(), &[PATCHED, 0, 0]].concat(),
        "rows of this field cannot be patches"
    )]
    #[case::invalid_value("target", 1, stored_target(7, &[0xff]), "invalid utf-8")]
    #[case::unsorted_keys(
        "target",
        2,
        [stored_target(9, b"x"), stored_target(8, b"y")].concat(),
        "keys are not strictly increasing"
    )]
    #[case::duplicate_keys(
        "target",
        2,
        [stored_target(9, b"x"), stored_target(9, b"y")].concat(),
        "keys are not strictly increasing"
    )]
    #[case::rows_beyond_the_body("target", 1 << 40, vec![], "target: truncated rows")]
    #[case::patch_ids_beyond_the_body(
        "rdeps",
        1,
        {
            let mut row = 7u64.to_le_bytes().to_vec();
            row.push(PATCHED);
            put(&mut row, 1 << 40);
            row
        },
        "truncated patch"
    )]
    #[case::patch_of_a_row_the_base_does_not_hold(
        "rdeps",
        1,
        [7u64.to_le_bytes().as_slice(), &[PATCHED, 0, 0]].concat(),
        "a patch needs a loaded row"
    )]
    fn a_malformed_body_is_rejected(
        #[case] field: &str,
        #[case] rows: u64,
        #[case] row_bytes: Vec<u8>,
        #[case] message: &str,
    ) {
        let base_bytes = snapshot_bytes(&sample_graph());
        let rebuilt = read(&base_bytes);
        let delta = delta_with_body(&body_with(&rebuilt, field, rows, &row_bytes));
        let error = rebuilt.apply_delta(&delta, IDENTITY).unwrap_err();
        assert!(format!("{error:#}").contains(message), "{error:#}");
        assert_eq!(snapshot_bytes(&rebuilt), base_bytes);
    }

    /// A zstd frame descriptor: an 8-byte content size, a single segment, and
    /// a checksum.
    const DESCRIPTOR: u8 = 0b1110_0100;

    #[test]
    fn a_frame_that_declares_an_oversized_body_is_rejected() {
        let base_bytes = snapshot_bytes(&sample_graph());
        let mut delta = delta_with_body(&[]);
        delta.truncate(HEADER_BYTES + IDENTITY.len());
        // A zstd frame: magic; a descriptor with an 8-byte content size, a
        // single segment and a checksum; the size; one empty last raw block;
        // the checksum.
        delta.extend_from_slice(&MAGICNUMBER.to_le_bytes());
        delta.push(DESCRIPTOR);
        delta.extend_from_slice(&(MAX_BODY_BYTES + 1).to_le_bytes());
        delta.extend_from_slice(&[1, 0, 0]);
        delta.extend_from_slice(&[0; 4]);
        let error = read(&base_bytes).apply_delta(&delta, IDENTITY).unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!("exceeds {MAX_BODY_BYTES}")),
            "{error:#}"
        );
    }

    #[test]
    fn an_empty_base_identity_is_rejected() {
        let base_bytes = snapshot_bytes(&sample_graph());
        let changed = read(&base_bytes);
        mutate(&changed);
        let error = changed.write_delta(b"", &mut Vec::new()).unwrap_err();
        assert!(
            format!("{error:#}").contains("identity is empty"),
            "{error:#}"
        );
        let (base_bytes, delta) = written_delta();
        let error = read(&base_bytes).apply_delta(&delta, b"").unwrap_err();
        assert!(
            format!("{error:#}").contains("identity is empty"),
            "{error:#}"
        );
    }

    #[test]
    fn a_label_set_that_no_target_uses_can_be_removed() -> anyhow::Result<()> {
        let base = TargetGraph::new();
        let only = base.store_target("fbcode//only:only");
        base.store_minimized_target(
            only,
            MinimizedBuckTarget {
                rule_type: base.store_rule_type("rule"),
                oncall: None,
                labels: vec![base.store_label("only")],
                target_hash: TargetHash::new(&format!("{:032x}", 1)),
            },
        );
        let base_bytes = snapshot_bytes(&base);
        let changed = read(&base_bytes);
        let label_set = changed
            .minimized_targets
            .get(&only)
            .expect("target")
            .value()
            .label_set;
        changed.remove_targets_batch(&[only].into_iter().collect());
        changed.label_set_id_to_labels.discard(&label_set);
        let (_, summary) = round_trip(&base_bytes, &changed)?;
        let removed = |name| {
            summary
                .fields
                .iter()
                .find(|f| f.name == name)
                .unwrap()
                .removed
        };
        assert_eq!((removed("minimized"), removed("labelsets")), (1, 1));
        Ok(())
    }
}
