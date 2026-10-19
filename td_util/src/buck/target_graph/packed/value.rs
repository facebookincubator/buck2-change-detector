/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt::Debug;
use std::io::Write;

use anyhow::ensure;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::super::StoredMinimizedTarget;
use super::INDEX_BYTES;
use super::Key;
use super::View;
use super::WORD_BYTES;
use crate::types::TargetHash;

const INLINE_IDS: usize = 16;
const IDS_PER_WRITE_BATCH: usize = 1024;

pub(in super::super) trait Value:
    Clone + Debug + PartialEq + Send + Sync + Serialize + DeserializeOwned + 'static
{
    fn encode(&self, out: &mut dyn Write) -> std::io::Result<()>;
    fn validate(bytes: &[u8]) -> anyhow::Result<()>;
    /// Only called on immutable rows whose shape and value were validated.
    fn decode(bytes: &[u8]) -> Self;
    fn decode_view(view: View<'_>) -> Self {
        assert!(view.ids.is_none(), "dictionary only applies to ID lists");
        Self::decode(view.bytes)
    }
    fn validate_view(view: View<'_>) -> anyhow::Result<()> {
        ensure!(view.ids.is_none(), "dictionary only applies to ID lists");
        Self::validate(view.bytes)
    }
}

impl Value for String {
    fn encode(&self, out: &mut dyn Write) -> std::io::Result<()> {
        out.write_all(self.as_bytes())
    }
    fn validate(bytes: &[u8]) -> anyhow::Result<()> {
        std::str::from_utf8(bytes)?;
        Ok(())
    }
    fn decode(bytes: &[u8]) -> Self {
        std::str::from_utf8(bytes)
            .expect("validated string")
            .to_owned()
    }
}
impl<K: Key> Value for Vec<K> {
    fn decode_view(view: View<'_>) -> Self {
        view_ids(view).collect()
    }
    fn validate_view(view: View<'_>) -> anyhow::Result<()> {
        if let Some(ids) = view.ids {
            ensure!(
                view.bytes.len().is_multiple_of(INDEX_BYTES),
                "packed index sequence length"
            );
            for bytes in view.bytes.as_chunks::<INDEX_BYTES>().0 {
                ensure!(
                    (u32::from_le_bytes(*bytes) as usize) < ids.len(),
                    "packed ID index out of range"
                );
            }
            Ok(())
        } else {
            Self::validate(view.bytes)
        }
    }
    fn encode(&self, out: &mut dyn Write) -> std::io::Result<()> {
        // Most adjacency lists are tiny. Do not initialize an 8 KiB buffer for
        // every single-edge row; retain large writes for high-fanout targets.
        if self.len() <= INLINE_IDS {
            let mut buffer = [0; INLINE_IDS * WORD_BYTES];
            for (value, bytes) in self
                .iter()
                .zip(buffer.as_chunks_mut::<WORD_BYTES>().0.iter_mut())
            {
                bytes.copy_from_slice(&value.raw().to_le_bytes());
            }
            return out.write_all(&buffer[..self.len() * WORD_BYTES]);
        }
        let mut buffer = [0; IDS_PER_WRITE_BATCH * WORD_BYTES];
        for chunk in self.chunks(IDS_PER_WRITE_BATCH) {
            for (value, bytes) in chunk
                .iter()
                .zip(buffer.as_chunks_mut::<WORD_BYTES>().0.iter_mut())
            {
                bytes.copy_from_slice(&value.raw().to_le_bytes());
            }
            out.write_all(&buffer[..chunk.len() * WORD_BYTES])?;
        }
        Ok(())
    }
    fn validate(bytes: &[u8]) -> anyhow::Result<()> {
        ensure!(
            bytes.len().is_multiple_of(WORD_BYTES),
            "native ID sequence length"
        );
        Ok(())
    }
    fn decode(bytes: &[u8]) -> Self {
        ids(bytes).collect()
    }
}
pub(super) fn ids<K: Key>(bytes: &[u8]) -> impl Iterator<Item = K> + '_ {
    bytes
        .as_chunks::<WORD_BYTES>()
        .0
        .iter()
        .map(|b| K::from_raw(u64::from_le_bytes(*b)))
}
pub(super) fn view_ids<K: Key>(view: View<'_>) -> impl Iterator<Item = K> + '_ {
    view.bytes
        .chunks_exact(if view.ids.is_some() {
            INDEX_BYTES
        } else {
            WORD_BYTES
        })
        .map(move |bytes| {
            let raw = if let Some(ids) = view.ids {
                ids[u32::from_le_bytes(bytes.try_into().expect("validated index")) as usize]
            } else {
                u64::from_le_bytes(bytes.try_into().expect("validated ID"))
            };
            K::from_raw(raw)
        })
}

impl Value for Vec<String> {
    fn encode(&self, out: &mut dyn Write) -> std::io::Result<()> {
        out.write_all(&(self.len() as u64).to_le_bytes())?;
        for s in self {
            out.write_all(&(s.len() as u64).to_le_bytes())?;
            out.write_all(s.as_bytes())?;
        }
        Ok(())
    }
    fn validate(bytes: &[u8]) -> anyhow::Result<()> {
        use anyhow::Context;
        let count = super::word(bytes, 0)? as usize;
        ensure!(count <= bytes.len() / WORD_BYTES, "native string count");
        let mut offset = WORD_BYTES;
        for _ in 0..count {
            let n = super::word(bytes, offset)? as usize;
            offset = offset
                .checked_add(WORD_BYTES)
                .context("string offset overflow")?;
            let end = offset.checked_add(n).context("string length overflow")?;
            std::str::from_utf8(bytes.get(offset..end).context("short native string")?)?;
            offset = end;
        }
        ensure!(offset == bytes.len(), "trailing string sequence bytes");
        Ok(())
    }
    fn decode(bytes: &[u8]) -> Self {
        let n = super::word(bytes, 0).expect("validated count");
        let mut offset = WORD_BYTES;
        (0..n)
            .map(|_| {
                let n = super::word(bytes, offset).expect("validated length") as usize;
                offset += WORD_BYTES;
                let s = String::decode(&bytes[offset..offset + n]);
                offset += n;
                s
            })
            .collect()
    }
}
impl Value for () {
    fn encode(&self, _: &mut dyn Write) -> std::io::Result<()> {
        Ok(())
    }
    fn validate(bytes: &[u8]) -> anyhow::Result<()> {
        ensure!(bytes.is_empty(), "native set payload");
        Ok(())
    }
    fn decode(_: &[u8]) -> Self {}
}

const RULE_TYPE_OFFSET: usize = 0;
const ONCALL_OFFSET: usize = RULE_TYPE_OFFSET + WORD_BYTES;
const LABEL_SET_OFFSET: usize = ONCALL_OFFSET + WORD_BYTES;
const ONCALL_PRESENT_OFFSET: usize = LABEL_SET_OFFSET + WORD_BYTES;
const HASH_TAG_OFFSET: usize = ONCALL_PRESENT_OFFSET + 1;
const HASH_OFFSET: usize = HASH_TAG_OFFSET + 1;
const HEX_HASH_TAG: u8 = 0;
const TEXT_HASH_TAG: u8 = 1;

impl StoredMinimizedTarget {
    pub(in super::super) const PACKED_HEX_SIZE: usize = HASH_OFFSET + size_of::<u128>();
}

impl Value for StoredMinimizedTarget {
    fn encode(&self, out: &mut dyn Write) -> std::io::Result<()> {
        out.write_all(&self.rule_type.raw().to_le_bytes())?;
        out.write_all(&self.oncall.map_or(0, |v| v.raw()).to_le_bytes())?;
        out.write_all(&self.label_set.raw().to_le_bytes())?;
        if let Some(bytes) = self.target_hash.as_hex128() {
            out.write_all(&[u8::from(self.oncall.is_some()), HEX_HASH_TAG])?;
            out.write_all(bytes)
        } else {
            out.write_all(&[u8::from(self.oncall.is_some()), TEXT_HASH_TAG])?;
            self.target_hash.with_str(|s| out.write_all(s.as_bytes()))
        }
    }
    fn validate(bytes: &[u8]) -> anyhow::Result<()> {
        ensure!(
            bytes.len() >= HASH_OFFSET && bytes[ONCALL_PRESENT_OFFSET] <= 1,
            "native target shape"
        );
        ensure!(
            bytes[ONCALL_PRESENT_OFFSET] != 0 || super::word(bytes, ONCALL_OFFSET)? == 0,
            "absent native oncall"
        );
        match bytes[HASH_TAG_OFFSET] {
            HEX_HASH_TAG => ensure!(
                bytes.len() == Self::PACKED_HEX_SIZE,
                "packed hex hash length"
            ),
            TEXT_HASH_TAG => {
                std::str::from_utf8(&bytes[HASH_OFFSET..])?;
            }
            _ => anyhow::bail!("packed hash tag"),
        }
        Ok(())
    }
    fn decode(bytes: &[u8]) -> Self {
        Self {
            rule_type: super::super::RawId::from_raw(
                super::word(bytes, RULE_TYPE_OFFSET).expect("validated rule"),
            ),
            oncall: (bytes[ONCALL_PRESENT_OFFSET] == 1).then(|| {
                super::super::RawId::from_raw(
                    super::word(bytes, ONCALL_OFFSET).expect("validated oncall"),
                )
            }),
            label_set: super::super::RawId::from_raw(
                super::word(bytes, LABEL_SET_OFFSET).expect("validated labels"),
            ),
            target_hash: if bytes[HASH_TAG_OFFSET] == HEX_HASH_TAG {
                TargetHash::from_hex128(bytes[HASH_OFFSET..].try_into().expect("validated hash"))
            } else {
                TargetHash::new(std::str::from_utf8(&bytes[HASH_OFFSET..]).expect("validated hash"))
            },
        }
    }
}
