/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

use anyhow::Context;

use crate::json::BUFFER_SIZE;
use crate::zstd::has_zstd_magic;
use crate::zstd::is_zstd;

/// Creates or truncates `file_path` and opens it for writing; a `.zst` path
/// is compressed as one zstd frame by the writing thread. The same as
/// [`file_writer_with_zstd_threads`] with [`ZstdThreads::Writer`].
pub fn file_writer(file_path: &Path) -> anyhow::Result<Box<dyn Write>> {
    file_writer_with_zstd_threads(file_path, ZstdThreads::Writer)
}

/// Buffer in front of a multithreaded zstd encoder (64 KiB).
const ZSTD_WORKER_WRITE_BUFFER_SIZE: usize = 64 * 1024;

/// The most worker threads that compress one output: a chosen limit on the
/// threads and job buffers of one encoder.
pub const MAX_ZSTD_WORKERS: usize = 16;

/// The threads that compress a `.zst` output. An output whose path does not
/// end in `.zst` ignores it.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum ZstdThreads {
    /// The thread that writes compresses.
    Writer,
    /// Worker threads compress: one per thread of the rayon pool that is
    /// current when the writer is created, at most [`MAX_ZSTD_WORKERS`]. On a
    /// one-thread pool, or when zstd cannot start workers (a warning is
    /// logged), the writing thread compresses. For a `.zst` output created
    /// outside any rayon pool, reading the pool size starts rayon's global
    /// pool.
    Pool,
}

impl ZstdThreads {
    /// zstd's worker-thread count (`nbWorkers`): 0 means the writing thread
    /// compresses.
    fn zstd_workers(self) -> u32 {
        match self {
            Self::Writer => 0,
            Self::Pool => match rayon::current_num_threads().min(MAX_ZSTD_WORKERS) {
                1 => 0,
                threads => threads as u32,
            },
        }
    }
}

/// Creates or truncates `file_path` and opens it for writing. A `.zst` path
/// is compressed as one zstd frame by `threads`; the choice of threads
/// changes only the compressed bytes, not what a reader decompresses.
pub fn file_writer_with_zstd_threads(
    file_path: &Path,
    threads: ZstdThreads,
) -> anyhow::Result<Box<dyn Write>> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(file_path)
        .with_context(|| format!("Unable to open file `{}` for writing", file_path.display()))?;

    if is_zstd(file_path) {
        let mut encoder = zstd::Encoder::new(file, zstd::DEFAULT_COMPRESSION_LEVEL)?;
        let workers = threads.zstd_workers();
        if workers > 0 {
            match encoder.multithread(workers) {
                // The encoder copies its input into job buffers, so a
                // large block goes to it directly: a `BufWriter` passes
                // through any write at least its own size, and a small
                // buffer only gathers writes smaller than that.
                Ok(()) => {
                    return Ok(Box::new(BufWriter::with_capacity(
                        ZSTD_WORKER_WRITE_BUFFER_SIZE,
                        encoder.auto_finish(),
                    )));
                }
                // A zstd without worker support still compresses correctly.
                Err(error) => tracing::warn!(
                    "zstd cannot compress `{}` on {workers} threads, using one: {error}",
                    file_path.display()
                ),
            }
        }
        Ok(Box::new(BufWriter::with_capacity(
            BUFFER_SIZE,
            encoder.auto_finish(),
        )))
    } else {
        Ok(Box::new(BufWriter::with_capacity(BUFFER_SIZE, file)))
    }
}

pub fn file_reader(file_path: &Path) -> anyhow::Result<Box<dyn BufRead + Send>> {
    let file = File::open(file_path)
        .with_context(|| format!("Unable to open file `{}` for reading", file_path.display()))?;

    if is_zstd(file_path) || has_zstd_magic(file_path) {
        let decoder = zstd::Decoder::new(file)?;
        Ok(Box::new(BufReader::with_capacity(BUFFER_SIZE, decoder)))
    } else {
        Ok(Box::new(BufReader::with_capacity(BUFFER_SIZE, file)))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SerializationFormat {
    Json,
    JsonLines,
    Bincode,
}

pub fn detect_format_from_path(path: &Path) -> anyhow::Result<SerializationFormat> {
    let mut check_path = path;

    // Handle .zst extension by checking the file stem
    if path.extension().and_then(|s| s.to_str()) == Some("zst") {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            check_path = Path::new(stem);
        }
    }

    match check_path.extension().and_then(|s| s.to_str()) {
        Some("json") => Ok(SerializationFormat::Json),
        Some("jsonl") => Ok(SerializationFormat::JsonLines),
        Some("bin") => Ok(SerializationFormat::Bincode),
        _ => Err(anyhow::anyhow!(
            "Unknown format for path: {}. Supported: .json, .jsonl, .bin (with optional .zst compression)",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rstest::rstest;
    use tempfile::TempDir;

    use super::*;

    static DATA: &str = "Artifact data";

    #[test]
    pub fn test_write_success() {
        let out_dir = TempDir::new().unwrap();
        let out_path = out_dir.path().join("test_artifact.json");

        file_writer(&out_path)
            .unwrap()
            .write_all(DATA.as_bytes())
            .unwrap();

        let written = fs::read_to_string(&out_path).unwrap();
        assert_eq!(written, DATA);
    }

    #[test]
    pub fn test_write_error() {
        assert!(file_writer(Path::new("/invalid/file/path")).is_err());
    }

    #[test]
    pub fn test_zstd_encoding() {
        let out_dir = TempDir::new().unwrap();
        let out_path = out_dir.path().join("test_artifact.zst");

        file_writer(&out_path)
            .unwrap()
            .write_all(DATA.as_bytes())
            .unwrap();

        assert!(out_path.exists());
        let compressed_data = fs::read(&out_path).unwrap();
        assert!(!compressed_data.is_empty());

        let file = fs::File::open(&out_path).unwrap();
        let mut decoder = zstd::Decoder::new(file).unwrap();
        let mut decompressed = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut decompressed).unwrap();
        assert_eq!(decompressed, DATA);
    }

    /// Runs `op` on a rayon pool of `threads` threads.
    fn in_rayon_pool<R: Send>(threads: usize, op: impl FnOnce() -> R + Send) -> R {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(op)
    }

    /// `Pool` compresses on one worker per rayon thread, up to the cap;
    /// `Writer` compresses on the writing thread.
    #[rstest]
    #[case::writer(ZstdThreads::Writer, 4, 0)]
    #[case::pool_of_one_thread(ZstdThreads::Pool, 1, 0)]
    #[case::pool_below_the_cap(ZstdThreads::Pool, 4, 4)]
    #[case::pool_above_the_cap(ZstdThreads::Pool, MAX_ZSTD_WORKERS + 1, MAX_ZSTD_WORKERS as u32)]
    fn test_zstd_threads_follow_the_rayon_pool_up_to_the_cap(
        #[case] threads: ZstdThreads,
        #[case] pool_threads: usize,
        #[case] expected_workers: u32,
    ) {
        assert_eq!(
            in_rayon_pool(pool_threads, || threads.zstd_workers()),
            expected_workers,
            "{threads:?} on a pool of {pool_threads} threads"
        );
    }

    #[test]
    pub fn test_zstd_threads_pool_output_round_trips() {
        let out_dir = TempDir::new().unwrap();
        let out_path = out_dir.path().join("test_artifact.zst");
        // About 26 MB: more than three zstd jobs (8 MiB each at the default
        // level), so the workers split the frame.
        let payload = DATA.repeat(2_000_000);

        in_rayon_pool(4, || -> anyhow::Result<()> {
            let mut writer = file_writer_with_zstd_threads(&out_path, ZstdThreads::Pool)?;
            writer.write_all(payload.as_bytes())?;
            writer.flush()?;
            Ok(())
        })
        .unwrap();

        let mut reader = file_reader(&out_path).unwrap();
        let mut decompressed = String::new();
        std::io::Read::read_to_string(&mut reader, &mut decompressed).unwrap();
        assert_eq!(decompressed, payload);
    }

    #[test]
    pub fn test_read_success() {
        let out_dir = TempDir::new().unwrap();
        let out_path = out_dir.path().join("test_artifact.json");

        // Write data first
        file_writer(&out_path)
            .unwrap()
            .write_all(DATA.as_bytes())
            .unwrap();

        // Read it back
        let mut reader = file_reader(&out_path).unwrap();
        let mut read_data = String::new();
        std::io::Read::read_to_string(&mut reader, &mut read_data).unwrap();
        assert_eq!(read_data, DATA);
    }

    #[test]
    pub fn test_read_error() {
        assert!(file_reader(Path::new("/invalid/file/path")).is_err());
    }

    #[test]
    pub fn test_zstd_decoding() {
        let out_dir = TempDir::new().unwrap();
        let out_path = out_dir.path().join("test_artifact.zst");

        // Write compressed data
        file_writer(&out_path)
            .unwrap()
            .write_all(DATA.as_bytes())
            .unwrap();

        // Read compressed data back
        let mut reader = file_reader(&out_path).unwrap();
        let mut decompressed = String::new();
        std::io::Read::read_to_string(&mut reader, &mut decompressed).unwrap();
        assert_eq!(decompressed, DATA);
    }

    #[test]
    pub fn test_round_trip_write_read() {
        let out_dir = TempDir::new().unwrap();
        let out_path = out_dir.path().join("test_roundtrip.json");

        // Write data
        let mut writer = file_writer(&out_path).unwrap();
        writer.write_all(DATA.as_bytes()).unwrap();
        drop(writer); // Ensure file is closed

        // Read data back
        let mut reader = file_reader(&out_path).unwrap();
        let mut read_data = String::new();
        std::io::Read::read_to_string(&mut reader, &mut read_data).unwrap();

        assert_eq!(read_data, DATA);
    }

    #[test]
    pub fn test_round_trip_write_read_compressed() {
        let out_dir = TempDir::new().unwrap();
        let out_path = out_dir.path().join("test_roundtrip.zst");

        // Write compressed data
        let mut writer = file_writer(&out_path).unwrap();
        writer.write_all(DATA.as_bytes()).unwrap();
        drop(writer); // Ensure file is closed

        // Read compressed data back
        let mut reader = file_reader(&out_path).unwrap();
        let mut decompressed = String::new();
        std::io::Read::read_to_string(&mut reader, &mut decompressed).unwrap();

        assert_eq!(decompressed, DATA);
    }
}
