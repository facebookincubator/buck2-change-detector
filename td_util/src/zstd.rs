/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Utilities for working with zstd compressed files.

use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::path::Path;

use anyhow::Context;

/// Zstd frame magic number per RFC 8878 Section 3.1.1
/// https://datatracker.ietf.org/doc/html/rfc8878#section-3.1.1
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Cap on zstd worker threads. Above ~16 threads, gains diminish and
/// contention with co-tenant workloads on shared sandcastle hosts becomes the
/// larger cost.
const ZSTD_MAX_WORKERS: u32 = 16;

/// Check if a file has a zstd extension (.zst)
pub fn is_zstd(filename: &Path) -> bool {
    match filename.extension() {
        Some(x) => x == "zst",
        None => false,
    }
}

/// Check if a file contains zstd-compressed data by reading its magic bytes.
/// Returns true if the file starts with the zstd magic bytes.
pub fn has_zstd_magic(file_path: &Path) -> bool {
    let Ok(mut file) = File::open(file_path) else {
        return false;
    };

    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_ok() {
        magic == ZSTD_MAGIC
    } else {
        false
    }
}

/// Number of zstd worker threads per frame when compressing many independent
/// frames in parallel. Splits the host's parallelism budget across `frames`
/// so that 22 frames × workers ≤ available cores. Falls back to 1 (no MT
/// per frame) on small hosts or when parallelism cannot be detected.
pub fn frame_worker_count(frames: u32) -> u32 {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(0);
    (cores / frames.max(1)).clamp(1, ZSTD_MAX_WORKERS)
}

/// Worker counts for a bounded set of Rayon frame tasks. Reserves one caller
/// thread per active frame and distributes the remaining Rayon budget across
/// the frames. A zero entry selects zstd's single-threaded mode for that frame.
pub fn frame_worker_counts_with_callers(
    frames: u32,
    rayon_threads: u32,
    max_workers: u32,
) -> Vec<u32> {
    if frames == 0 {
        return Vec::new();
    }

    // Multithreaded zstd workers run in addition to each Rayon caller. Reserve
    // those callers and give every active frame the same bounded worker count.
    let max_workers = max_workers.clamp(1, ZSTD_MAX_WORKERS);
    let worker_budget = rayon_threads
        .saturating_sub(frames)
        .min(frames.saturating_mul(max_workers));
    vec![worker_budget / frames; frames as usize]
}

/// Compress `input` into a self-contained zstd frame at the default
/// compression level. When `workers > 1`, uses the multithreaded zstd
/// encoder; when `workers == 1`, uses the single-threaded `encode_all`
/// fast path.
pub fn zstd_encode_to_vec(input: &[u8], workers: u32) -> anyhow::Result<Vec<u8>> {
    if workers <= 1 {
        return zstd::encode_all(input, zstd::DEFAULT_COMPRESSION_LEVEL).context("zstd encode_all");
    }
    let mut output = Vec::new();
    let mut encoder = zstd::Encoder::new(&mut output, zstd::DEFAULT_COMPRESSION_LEVEL)
        .context("zstd::Encoder::new")?;
    encoder
        .multithread(workers)
        .context("zstd encoder.multithread")?;
    encoder.write_all(input).context("zstd encoder write_all")?;
    encoder.finish().context("zstd encoder finish")?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[test]
    fn zstd_encode_to_vec_round_trips_st() {
        let payload = b"hello world".repeat(1024);
        let compressed = zstd_encode_to_vec(&payload, 1).unwrap();
        assert!(!compressed.is_empty());
        assert_ne!(compressed, payload);
        let decompressed = zstd::decode_all(&compressed[..]).unwrap();
        assert_eq!(decompressed, payload);
    }

    #[test]
    fn zstd_encode_to_vec_round_trips_mt() {
        let payload = b"hello world".repeat(1024);
        let compressed = zstd_encode_to_vec(&payload, 4).unwrap();
        let decompressed = zstd::decode_all(&compressed[..]).unwrap();
        assert_eq!(decompressed, payload);
    }

    #[test]
    fn frame_worker_count_clamps() {
        assert!(frame_worker_count(22) >= 1);
        assert!(frame_worker_count(22) <= ZSTD_MAX_WORKERS);
        assert_eq!(frame_worker_count(u32::MAX), 1);
    }

    #[rstest]
    #[case(0, 16, 2, vec![])]
    #[case(4, 4, 2, vec![0; 4])]
    #[case(4, 7, 2, vec![0; 4])]
    #[case(4, 10, 2, vec![1; 4])]
    #[case(16, 316, 2, vec![2; 16])]
    fn frame_worker_counts_follow_the_supplied_budget(
        #[case] frames: u32,
        #[case] rayon_threads: u32,
        #[case] max_workers: u32,
        #[case] expected: Vec<u32>,
    ) {
        assert_eq!(
            frame_worker_counts_with_callers(frames, rayon_threads, max_workers),
            expected
        );
    }
}
