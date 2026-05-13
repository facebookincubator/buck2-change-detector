/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Utilities for working with JSON and JSON-lines files.

use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::path::Path;

use anyhow::Context;
use itertools::Itertools;
use rayon::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::zstd::has_zstd_magic;
use crate::zstd::is_zstd;

/// Buffer size for reading files (10MB)
pub const BUFFER_SIZE: usize = 10 * 1024 * 1024;

// Function definition mostly to get the error types to line up
fn parse_line<T: for<'a> Deserialize<'a>>(x: Result<String, io::Error>) -> anyhow::Result<T> {
    let x = x?;
    serde_json::from_str(&x).with_context(|| format!("When parsing: {x}"))
}

fn open_file(filename: &Path) -> anyhow::Result<Box<dyn Read + Send>> {
    let file = File::open(filename)?;
    if is_zstd(filename) || has_zstd_magic(filename) {
        Ok(Box::new(zstd::Decoder::new(file)?))
    } else {
        Ok(Box::new(file))
    }
}

/// Read a file that consists of many JSON blobs, one per line.
/// Preserves the order of items from the input file.
pub fn read_file_lines_parallel_ordered<T: for<'a> Deserialize<'a> + Send>(
    filename: &Path,
) -> anyhow::Result<Vec<T>> {
    let inner = || -> anyhow::Result<Vec<T>> {
        let file = open_file(filename)?;
        // 10MB buffer
        let rdr = BufReader::with_capacity(BUFFER_SIZE, file);
        let chunk_size = 5000;
        let mut results = Vec::new();

        for lines_chunk in &rdr.lines().chunks(chunk_size) {
            let lines_vec: Vec<_> = lines_chunk.collect();
            let chunk_results = lines_vec
                .into_par_iter()
                .map(parse_line)
                .collect::<Result<Vec<_>, _>>()?;
            results.extend(chunk_results);
        }

        Ok(results)
    };
    inner().with_context(|| format!("When reading file `{}`", filename.display()))
}

/// Read a file that consists of many JSON blobs, one per line.
/// The order of the entries is not guaranteed.
/// ~25% faster than ordered version above.
pub fn read_file_lines_parallel<T: for<'a> Deserialize<'a> + Send>(
    filename: &Path,
) -> anyhow::Result<Vec<T>> {
    let inner = || -> anyhow::Result<Vec<T>> {
        read_file_lines_par_iter(filename)?.collect::<anyhow::Result<Vec<T>>>()
    };
    inner().with_context(|| format!("When reading file `{}`", filename.display()))
}

/// Returns an unordered parallel iterator over the parsed lines.
/// Convenience function to avoid unnecessary allocations for when further processing is needed.
pub fn read_file_lines_par_iter<T: for<'a> Deserialize<'a> + Send>(
    filename: &Path,
) -> anyhow::Result<impl ParallelIterator<Item = anyhow::Result<T>> + use<T>> {
    let file = open_file(filename)?;
    // 10MB buffer
    let rdr = BufReader::with_capacity(BUFFER_SIZE, file);

    Ok(rdr.lines().par_bridge().map(parse_line::<T>))
}

/// Read JSON lines from a reader. The order of entries is not guaranteed.
/// Use this when you want explicit control over file opening (e.g., with file_io::file_reader).
pub fn read_reader_lines_parallel<T: for<'a> Deserialize<'a> + Send>(
    reader: impl BufRead + Send,
) -> anyhow::Result<Vec<T>> {
    reader
        .lines()
        .par_bridge()
        .map(parse_line::<T>)
        .collect::<anyhow::Result<Vec<T>>>()
}

pub fn read_reader_lines_parallel_bytes<R, T>(mut reader: R) -> anyhow::Result<Vec<T>>
where
    R: Read + Send,
    T: for<'a> Deserialize<'a> + Send,
{
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .context("Failed to read input bytes for parallel JSON-lines parsing")?;
    bytes
        .par_split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice::<T>(line)
                .with_context(|| format!("When parsing line: {}", String::from_utf8_lossy(line)))
        })
        .collect::<anyhow::Result<Vec<T>>>()
}

/// Like [`read_reader_lines_parallel_bytes`], but decompresses the input in
/// fixed-size chunks on a dedicated producer thread and parses each chunk's
/// lines on a rayon worker pool, so decompression and parsing overlap.
///
/// Compared to the slurp-then-parse variant this:
/// - bounds peak memory at roughly `chunk_size * channel_depth + |Vec<T>|`
///   instead of the full decompressed size + `|Vec<T>|`, and
/// - hides decompression latency behind parsing -- on workloads where the
///   parser pool is the bottleneck, total wall time approaches
///   `max(decompress_wall, parse_wall)` rather than their sum.
///
/// Each chunk's split-and-parse is sequential within the chunk (chunks are
/// small enough that the per-chunk parallelism in
/// [`read_reader_lines_parallel_bytes`] is unnecessary overhead), but
/// chunks are parsed in parallel via [`ParallelBridge`].
pub fn read_reader_lines_chunked_pipeline<R, T>(reader: R) -> anyhow::Result<Vec<T>>
where
    R: Read + Send,
    T: for<'a> Deserialize<'a> + Send,
{
    /// Size of each decompressed chunk handed off to a parser worker. Sized
    /// to keep the producer thread saturated without ballooning memory; one
    /// chunk per in-flight parser plus a couple in-channel is the steady
    /// state.
    const CHUNK_SIZE: usize = 16 * 1024 * 1024;
    /// Bounded so the decompressor blocks once parsers fall behind; keeps
    /// peak memory at roughly `CHUNK_SIZE * (CHANNEL_DEPTH + worker_count)`.
    const CHANNEL_DEPTH: usize = 8;

    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(CHANNEL_DEPTH);

    std::thread::scope(|s| {
        let producer = s.spawn(move || -> anyhow::Result<()> {
            let mut reader = reader;
            let mut leftover: Vec<u8> = Vec::new();
            loop {
                // Each chunk is its own owned Vec so workers can move it across
                // thread boundaries and drop it independently.
                let mut buf = Vec::with_capacity(CHUNK_SIZE + leftover.len());
                buf.extend_from_slice(&leftover);
                leftover.clear();
                let start = buf.len();
                buf.resize(start + CHUNK_SIZE, 0);

                let mut filled = 0;
                while filled < CHUNK_SIZE {
                    match reader.read(&mut buf[start + filled..]) {
                        Ok(0) => break,
                        Ok(n) => filled += n,
                        Err(e) => {
                            return Err(anyhow::Error::from(e).context(
                                "Failed to read chunk during pipelined JSON-lines parsing",
                            ));
                        }
                    }
                }
                buf.truncate(start + filled);

                let at_eof = filled < CHUNK_SIZE;

                if buf.is_empty() {
                    return Ok(());
                }

                // If we filled the chunk, the last line probably spans the
                // chunk boundary; carry the trailing partial line into the
                // next chunk so each emitted chunk ends on a newline (or EOF).
                if !at_eof {
                    if let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') {
                        leftover.extend_from_slice(&buf[last_nl + 1..]);
                        buf.truncate(last_nl + 1);
                    }
                    // else: the entire chunk is one record with no newline.
                    // Forward as-is; the parser will return an error if it's
                    // truly malformed, but typically an oversized record just
                    // means the next chunk will append more bytes -- not
                    // representable in this design without unbounded buffering,
                    // so accept the failure mode.
                }

                // Receiver dropped means consumers gave up (e.g. parse error).
                if tx.send(buf).is_err() {
                    return Ok(());
                }

                if at_eof {
                    return Ok(());
                }
            }
        });

        let parsed: anyhow::Result<Vec<T>> = rx
            .into_iter()
            .par_bridge()
            .flat_map_iter(|chunk| {
                // Split + parse sequentially within a single chunk: the chunk is
                // small (16 MB) so the rayon overhead of nested parallelism would
                // dominate. Parallelism is across chunks, not within them.
                chunk
                    .split(|&b| b == b'\n')
                    .filter(|line| !line.is_empty())
                    .map(|line| {
                        serde_json::from_slice::<T>(line).with_context(|| {
                            format!("When parsing line: {}", String::from_utf8_lossy(line))
                        })
                    })
                    // Materialise into a Vec so the closure no longer borrows
                    // from `chunk` once we return; the chunk is dropped here.
                    .collect::<Vec<_>>()
                    .into_iter()
            })
            .collect();

        let producer_result = producer
            .join()
            .map_err(|_| anyhow::anyhow!("BTD chunk reader thread panicked"))?;

        // Surface a parse failure first (it triggered the producer to stop), but
        // if parsing succeeded ensure we still report any IO error from the
        // producer.
        let parsed = parsed?;
        producer_result?;
        Ok(parsed)
    })
}

/// Read a file that consists of many JSON blobs, one per line.
pub fn read_file_lines<T: for<'a> Deserialize<'a>>(filename: &Path) -> anyhow::Result<Vec<T>> {
    fn f<T: for<'a> Deserialize<'a>>(filename: &Path) -> anyhow::Result<Vec<T>> {
        let file = open_file(filename)?;
        let rdr = BufReader::with_capacity(BUFFER_SIZE, file);
        let mut res = Vec::new();
        for line in rdr.lines() {
            res.push(parse_line(line)?)
        }
        Ok(res)
    }

    f(filename).with_context(|| format!("When reading file `{}`", filename.display()))
}

/// Write out information as a list of JSON lines.
pub fn write_json_lines<W: Write, T: Serialize>(
    out: W,
    xs: impl IntoIterator<Item = T>,
) -> anyhow::Result<()> {
    let mut writer = BufWriter::with_capacity(BUFFER_SIZE, out);
    for x in xs.into_iter() {
        serde_json::to_writer(&mut writer, &x)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

/// Write out information as a JSON array, but make each entry in the array take up a single item.
pub fn write_json_per_line<W: Write, T: Serialize>(
    mut out: W,
    xs: impl IntoIterator<Item = T>,
) -> anyhow::Result<()> {
    let mut it = xs.into_iter();

    out.write_all(b"[")?;
    if let Some(first) = it.next() {
        out.write_all(b"\n  ")?;
        serde_json::to_writer(&mut out, &first)?;
        for x in it {
            out.write_all(b",\n  ")?;
            serde_json::to_writer(&mut out, &x)?;
        }
        out.write_all(b"\n")?;
    }
    out.write_all(b"]\n")?;

    out.flush()?;
    Ok(())
}

/// Parse a single key-value pair
pub fn parse_key_val(s: &str) -> anyhow::Result<(String, String)> {
    match s.split_once('=') {
        None => Err(anyhow::anyhow!("invalid KEY=value: no `=` found in `{s}`")),
        Some((a, b)) => Ok((a.to_owned(), b.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use crate::json::read_file_lines;
    use crate::json::read_file_lines_parallel;
    use crate::json::read_file_lines_parallel_ordered;
    use crate::json::write_json_lines;
    use crate::json::write_json_per_line;

    #[test]
    fn test_json_lines() {
        let mut file = NamedTempFile::new().unwrap();
        let data: Vec<i32> = (0..100).collect();
        write_json_lines(file.as_file_mut(), &data).unwrap();

        // Check single-thread reading
        assert_eq!(read_file_lines::<i32>(file.path()).unwrap(), data);

        // Check ordered parallel reading
        let ordered = read_file_lines_parallel_ordered::<i32>(file.path()).unwrap();
        assert_eq!(ordered, data);

        // Check unordered parallel reading
        let mut unordered = read_file_lines_parallel::<i32>(file.path()).unwrap();
        unordered.sort();
        assert_eq!(unordered, data);
    }

    #[test]
    fn test_json_per_line() {
        fn splat(data: &[i32]) -> String {
            let mut buffer = Vec::new();
            write_json_per_line(&mut buffer, data).unwrap();
            String::from_utf8(buffer).unwrap()
        }

        for i in 0..10 {
            let data: Vec<i32> = (0..i).collect();
            let res = splat(&data);
            assert_eq!(serde_json::from_str::<Vec<i32>>(&res).unwrap(), data);
            assert_eq!(res.lines().count(), if i == 0 { 1 } else { i as usize + 2 });
            assert!(res.ends_with('\n'));
        }

        assert_eq!(splat(&[]), "[]\n");
        assert_eq!(splat(&[1]), "[\n  1\n]\n");
        assert_eq!(splat(&[1, 2]), "[\n  1,\n  2\n]\n");
    }

    #[test]
    fn test_error_in_json_file() {
        let mut file = NamedTempFile::new().unwrap();
        let data: Vec<i32> = vec![0];

        // expect an int per line. add a string in the middle of the json file.
        write_json_lines(file.as_file_mut(), &data).unwrap();
        file.write_all(b"Not an i32\n").unwrap();
        write_json_lines(file.as_file_mut(), &data).unwrap();

        assert!(read_file_lines_parallel::<i32>(file.path()).is_err());
        assert!(read_file_lines_parallel_ordered::<i32>(file.path()).is_err());
        assert!(read_file_lines::<i32>(file.path()).is_err());
    }
}
