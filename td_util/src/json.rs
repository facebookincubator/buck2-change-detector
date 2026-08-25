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
use std::sync::Mutex;

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

/// Like [`read_reader_lines_parallel_bytes`], but reads the input in
/// fixed-size chunks and parses each one on the rayon pool, so reading (and
/// decompression) overlaps with parsing.
///
/// Compared to the slurp-then-parse variant this:
/// - bounds peak memory at a fixed multiple of the chunk size rather than
///   the full decompressed size, and
/// - hides read latency behind parsing -- on workloads where the parser pool
///   is the bottleneck, total wall time approaches `max(read_wall,
///   parse_wall)` rather than their sum.
///
/// Records come back in arbitrary order. Use
/// [`read_file_lines_parallel_ordered`] if order matters.
pub fn read_reader_lines_chunked_pipeline<R, T>(reader: R) -> anyhow::Result<Vec<T>>
where
    R: Read + Send,
    T: for<'a> Deserialize<'a> + Send,
{
    let per_chunk = map_reader_chunks_pipeline(reader, |chunk| {
        json_lines(chunk)
            .map(|line| {
                serde_json::from_slice::<T>(line).with_context(|| {
                    format!("When parsing line: {}", String::from_utf8_lossy(line))
                })
            })
            .collect::<anyhow::Result<Vec<T>>>()
    })?;
    // `flatten().collect()` cannot size the result from a nested iterator, so
    // it grows by doubling and reallocates its way through every record. The
    // chunk lengths are already known here.
    let mut records = Vec::with_capacity(per_chunk.iter().map(Vec::len).sum());
    for chunk in per_chunk {
        records.extend(chunk);
    }
    Ok(records)
}

/// The non-empty lines of a chunk passed to [`map_reader_chunks_pipeline`].
pub fn json_lines(chunk: &[u8]) -> impl Iterator<Item = &[u8]> {
    chunk.split(|&b| b == b'\n').filter(|line| !line.is_empty())
}

/// Chunk-level form of [`read_reader_lines_chunked_pipeline`]: one result
/// per newline-terminated chunk, in arbitrary order. The calling thread reads
/// and hands each chunk to the pool as its own task.
///
/// Errors if a single record does not fit in a chunk, since there is no way
/// to hand a reducing closure half a record without it quietly counting the
/// halves as two.
pub fn map_reader_chunks_pipeline<R, T, F>(reader: R, per_chunk: F) -> anyhow::Result<Vec<T>>
where
    R: Read + Send,
    T: Send,
    F: Fn(&[u8]) -> anyhow::Result<T> + Sync,
{
    /// Buffer size, so one less than this is the longest record readable.
    const CHUNK_SIZE: usize = 16 * 1024 * 1024;
    /// Peak memory is `CHUNK_SIZE * BUFFERS_PER_THREAD * threads`.
    const BUFFERS_PER_THREAD: usize = 2;

    map_reader_chunks_sized(reader, CHUNK_SIZE, BUFFERS_PER_THREAD, per_chunk)
}

/// A chunk buffer on loan to a task. Returning it on drop rather than at the
/// end of the task keeps an unwind from leaking it, which would eventually
/// wedge the reader in the backpressure loop instead of propagating the panic.
struct Recycled<'a> {
    buf: Option<Vec<u8>>,
    free_tx: &'a std::sync::mpsc::SyncSender<Vec<u8>>,
}

impl Recycled<'_> {
    fn bytes(&self) -> &[u8] {
        self.buf.as_deref().expect("buffer is taken only on drop")
    }
}

impl Drop for Recycled<'_> {
    fn drop(&mut self) {
        if let Some(buf) = self.buf.take() {
            let _ = self.free_tx.try_send(buf);
        }
    }
}

fn map_reader_chunks_sized<R, T, F>(
    reader: R,
    chunk_size: usize,
    buffers_per_thread: usize,
    per_chunk: F,
) -> anyhow::Result<Vec<T>>
where
    R: Read + Send,
    T: Send,
    F: Fn(&[u8]) -> anyhow::Result<T> + Sync,
{
    let max_buffers = rayon::current_num_threads().max(1) * buffers_per_thread;
    // Holds every buffer that can exist, so handing one back never fails.
    let (free_tx, free_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(max_buffers);
    let mut allocated = 0usize;

    let parsed: Mutex<Vec<T>> = Mutex::new(Vec::new());
    let failed: Mutex<Option<anyhow::Error>> = Mutex::new(None);

    // Not `scope`, which would run this blocking read loop on a pool worker
    // the tasks it feeds are waiting for.
    rayon::in_place_scope(|scope| -> anyhow::Result<()> {
        let mut reader = reader;
        let mut leftover: Vec<u8> = Vec::new();
        let (parsed, failed, free_tx, per_chunk) = (&parsed, &failed, &free_tx, &per_chunk);
        loop {
            if failed
                .lock()
                .expect("chunk-parse mutex should not be poisoned")
                .is_some()
            {
                return Ok(());
            }
            let mut buf = match free_rx.try_recv().ok() {
                Some(buf) => buf,
                None if allocated < max_buffers => {
                    allocated += 1;
                    vec![0u8; chunk_size]
                }
                // Backpressure. Yield rather than block: the tasks that
                // return buffers need a worker to run on, and this thread
                // may be the last one.
                None => loop {
                    if let Ok(buf) = free_rx.try_recv() {
                        break buf;
                    }
                    match rayon::yield_now() {
                        Some(rayon::Yield::Executed) => {}
                        Some(rayon::Yield::Idle) => std::thread::yield_now(),
                        // Not a pool thread, so blocking costs the pool nothing.
                        None => {
                            break free_rx
                                .recv()
                                .map_err(|_| anyhow::anyhow!("free list closed during read"))?;
                        }
                    }
                },
            };
            // The leftover eats into this chunk rather than extending the
            // buffer past `chunk_size`, so the longest readable record does
            // not depend on where the previous one happened to end.
            let start = leftover.len();
            buf[..start].copy_from_slice(&leftover);
            leftover.clear();

            let mut filled = 0;
            while start + filled < buf.len() {
                match reader.read(&mut buf[start + filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) => {
                        return Err(anyhow::Error::from(e)
                            .context("Failed to read chunk during pipelined JSON-lines parsing"));
                    }
                }
            }
            let mut at_eof = start + filled < buf.len();
            let mut valid = start + filled;
            if valid == 0 {
                return Ok(());
            }

            // Carry the trailing partial line into the next chunk, so every
            // chunk handed to a task ends on a newline or EOF.
            if !at_eof {
                match buf[..valid].iter().rposition(|&b| b == b'\n') {
                    Some(last_nl) => {
                        leftover.extend_from_slice(&buf[last_nl + 1..valid]);
                        valid = last_nl + 1;
                    }
                    // No newline in a full buffer is either an oversized
                    // record or a final record ending on the boundary. A
                    // short read separates those everywhere else, but this
                    // read was not short, so ask for one more byte.
                    None => {
                        let mut probe = [0u8; 1];
                        match reader.read(&mut probe) {
                            Ok(0) => at_eof = true,
                            Ok(_) => {
                                return Err(anyhow::anyhow!(
                                    "record does not fit in a {chunk_size} byte chunk: read {valid} bytes with no newline"
                                ));
                            }
                            Err(e) => {
                                return Err(anyhow::Error::from(e).context(
                                    "Failed to read chunk during pipelined JSON-lines parsing",
                                ));
                            }
                        }
                    }
                }
            }

            scope.spawn(move |_| {
                let buf = Recycled {
                    buf: Some(buf),
                    free_tx,
                };
                match per_chunk(&buf.bytes()[..valid]) {
                    Ok(value) => parsed
                        .lock()
                        .expect("chunk-parse mutex should not be poisoned")
                        .push(value),
                    // First error wins; later ones are usually the same
                    // cause seen by a sibling chunk.
                    Err(e) => {
                        let mut slot = failed
                            .lock()
                            .expect("chunk-parse mutex should not be poisoned");
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                    }
                }
            });

            if at_eof {
                return Ok(());
            }
        }
    })?;

    if let Some(e) = failed
        .lock()
        .expect("chunk-parse mutex should not be poisoned")
        .take()
    {
        return Err(e);
    }
    let out = std::mem::take(
        &mut *parsed
            .lock()
            .expect("chunk-parse mutex should not be poisoned"),
    );
    Ok(out)
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

    use crate::json::json_lines;
    use crate::json::map_reader_chunks_sized;
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

    #[test]
    fn backpressure_path_does_not_deadlock() {
        let input: Vec<u8> = (0..20_000)
            .flat_map(|i| format!("{{\"n\":{i}}}\n").into_bytes())
            .collect();
        let counted = map_reader_chunks_sized(input.as_slice(), 64, 1, |chunk| {
            anyhow::Ok(json_lines(chunk).count())
        })
        .unwrap();
        assert_eq!(counted.iter().sum::<usize>(), 20_000);
    }

    #[test]
    fn records_survive_chunk_seams() {
        let input: Vec<u8> = (0..5_000u64)
            .flat_map(|i| format!("{{\"n\":{i}}}\n").into_bytes())
            .collect();
        let mut seen: Vec<u64> = map_reader_chunks_sized(input.as_slice(), 64, 1, |chunk| {
            json_lines(chunk)
                .map(|line| {
                    let v: serde_json::Value = serde_json::from_slice(line)?;
                    Ok(v["n"].as_u64().expect("n is a number"))
                })
                .collect::<anyhow::Result<Vec<u64>>>()
        })
        .unwrap()
        .into_iter()
        .flatten()
        .collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..5_000u64).collect::<Vec<u64>>());
    }

    #[test]
    fn per_chunk_error_surfaces_without_hanging() {
        let input: Vec<u8> = (0..5_000u64)
            .flat_map(|i| format!("{{\"n\":{i}}}\n").into_bytes())
            .collect();
        let err = map_reader_chunks_sized(input.as_slice(), 64, 1, |chunk| {
            if json_lines(chunk).any(|line| line.contains(&b'7')) {
                anyhow::bail!("boom");
            }
            anyhow::Ok(0usize)
        })
        .unwrap_err();
        assert!(err.to_string().contains("boom"), "got: {err}");
    }

    #[test]
    fn empty_input_yields_nothing() {
        let counted = map_reader_chunks_sized(&b""[..], 64, 1, |chunk| {
            anyhow::Ok(json_lines(chunk).count())
        })
        .unwrap();
        assert_eq!(counted.iter().sum::<usize>(), 0);
    }

    #[test]
    fn final_record_without_trailing_newline_is_read() {
        let counted = map_reader_chunks_sized(&b"{\"n\":1}\n{\"n\":2}"[..], 64, 1, |chunk| {
            anyhow::Ok(json_lines(chunk).count())
        })
        .unwrap();
        assert_eq!(counted.iter().sum::<usize>(), 2);
    }

    #[test]
    fn record_ending_exactly_on_the_chunk_boundary_is_read() {
        let record = format!("{{\"n\":\"{}\"}}", "x".repeat(55));
        assert_eq!(record.len() + 1, 64);
        let input = format!("{record}\n{record}\n");
        let counted = map_reader_chunks_sized(input.as_bytes(), 64, 1, |chunk| {
            anyhow::Ok(json_lines(chunk).count())
        })
        .unwrap();
        assert_eq!(counted.iter().sum::<usize>(), 2);
    }

    #[test]
    fn unterminated_final_record_filling_the_chunk_exactly_is_read() {
        let record = format!("{{\"n\":\"{}\"}}", "x".repeat(56));
        assert_eq!(record.len(), 64);
        let counted = map_reader_chunks_sized(record.as_bytes(), 64, 1, |chunk| {
            anyhow::Ok(json_lines(chunk).count())
        })
        .unwrap();
        assert_eq!(counted.iter().sum::<usize>(), 1);
    }

    #[test]
    fn record_leaving_no_room_for_its_newline_is_an_error() {
        let record = format!("{{\"n\":\"{}\"}}", "x".repeat(56));
        assert_eq!(record.len(), 64);
        let input = format!("{record}\n");
        let err = map_reader_chunks_sized(input.as_bytes(), 64, 1, |chunk| {
            anyhow::Ok(json_lines(chunk).count())
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("does not fit in a 64 byte chunk"),
            "got: {err}"
        );
    }

    #[test]
    fn unterminated_final_record_one_byte_over_the_chunk_is_an_error() {
        let record = format!("{{\"n\":\"{}\"}}", "x".repeat(57));
        assert_eq!(record.len(), 65);
        let err = map_reader_chunks_sized(record.as_bytes(), 64, 1, |chunk| {
            anyhow::Ok(json_lines(chunk).count())
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("does not fit in a 64 byte chunk"),
            "got: {err}"
        );
    }

    #[test]
    fn the_record_limit_does_not_depend_on_alignment() {
        let long = format!("{{\"n\":\"{}\"}}", "x".repeat(92));
        assert_eq!(long.len(), 100);
        let tail: String = (0..50).map(|i| format!("{{\"n\":{i}}}\n")).collect();
        let alone = format!("{long}\n{tail}");
        let after_a_short_line = format!("{{\"n\":1}}\n{long}\n{tail}");
        for (label, input) in [
            ("alone", &alone),
            ("after a short line", &after_a_short_line),
        ] {
            let got = map_reader_chunks_sized(input.as_bytes(), 64, 1, |chunk| {
                anyhow::Ok(json_lines(chunk).count())
            });
            assert!(
                got.is_err(),
                "{label}: expected the 100 byte record to be rejected"
            );
        }
    }

    #[test]
    fn a_panicking_chunk_propagates_rather_than_wedging_the_reader() {
        let input: Vec<u8> = (0..5_000)
            .flat_map(|i| format!("{{\"n\":{i}}}\n").into_bytes())
            .collect();
        let panicked = std::panic::catch_unwind(|| {
            map_reader_chunks_sized(input.as_slice(), 64, 1, |_| -> anyhow::Result<usize> {
                panic!("boom")
            })
        });
        assert!(panicked.is_err(), "expected the panic to propagate");
    }

    #[test]
    fn concurrent_readers_on_a_full_pool_do_not_deadlock() {
        let input: Vec<u8> = (0..20_000)
            .flat_map(|i| format!("{{\"n\":{i}}}\n").into_bytes())
            .collect();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let read = || {
            map_reader_chunks_sized(input.as_slice(), 64, 1, |chunk| {
                anyhow::Ok(json_lines(chunk).count())
            })
            .unwrap()
            .iter()
            .sum::<usize>()
        };
        let (a, b) = pool.install(|| rayon::join(read, read));
        assert_eq!((a, b), (20_000, 20_000));
    }
}
