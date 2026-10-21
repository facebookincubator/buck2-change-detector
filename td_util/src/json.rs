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
///
/// The file is read in chunks that the rayon pool parses while the next
/// chunk is read (see [`read_reader_lines_chunked_pipeline`]).
pub fn read_file_lines_parallel_ordered<T: for<'a> Deserialize<'a> + Send>(
    filename: &Path,
) -> anyhow::Result<Vec<T>> {
    let inner = || -> anyhow::Result<Vec<T>> {
        read_reader_lines_chunked_pipeline_ordered(open_file(filename)?)
    };
    inner().with_context(|| format!("When reading file `{}`", filename.display()))
}

/// Read a file that consists of many JSON blobs, one per line.
/// The order of the entries is not guaranteed.
pub fn read_file_lines_parallel<T: for<'a> Deserialize<'a> + Send>(
    filename: &Path,
) -> anyhow::Result<Vec<T>> {
    let inner =
        || -> anyhow::Result<Vec<T>> { read_reader_lines_chunked_pipeline(open_file(filename)?) };
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
    let per_chunk = map_reader_chunks_pipeline(reader, parse_chunk::<T>)?;
    Ok(concat_chunks(per_chunk))
}

/// [`read_reader_lines_chunked_pipeline`], with the records in the order of
/// the input: the chunks are parsed in parallel and concatenated in the order
/// they were read.
pub fn read_reader_lines_chunked_pipeline_ordered<R, T>(reader: R) -> anyhow::Result<Vec<T>>
where
    R: Read + Send,
    T: for<'a> Deserialize<'a> + Send,
{
    let mut per_chunk = map_reader_chunks_indexed(reader, parse_chunk::<T>)?;
    per_chunk.sort_unstable_by_key(|(index, _)| *index);
    Ok(concat_chunks(
        per_chunk.into_iter().map(|(_, chunk)| chunk).collect(),
    ))
}

fn parse_chunk<T: for<'a> Deserialize<'a>>(chunk: &[u8]) -> anyhow::Result<Vec<T>> {
    json_lines(chunk)
        .map(|line| {
            serde_json::from_slice::<T>(line)
                .with_context(|| format!("When parsing line: {}", String::from_utf8_lossy(line)))
        })
        .collect()
}

/// `flatten().collect()` cannot size the result from a nested iterator, so
/// it grows by doubling and reallocates its way through every record. The
/// chunk lengths are already known here.
fn concat_chunks<T>(per_chunk: Vec<Vec<T>>) -> Vec<T> {
    let mut records = Vec::with_capacity(per_chunk.iter().map(Vec::len).sum());
    for chunk in per_chunk {
        records.extend(chunk);
    }
    records
}

/// The non-empty lines of a chunk passed to [`map_reader_chunks_pipeline`].
pub fn json_lines(chunk: &[u8]) -> impl Iterator<Item = &[u8]> {
    chunk.split(|&b| b == b'\n').filter(|line| !line.is_empty())
}

/// Chunk-level form of [`read_reader_lines_chunked_pipeline`]: one result
/// per newline-terminated chunk, in arbitrary order. The calling thread reads
/// and hands each chunk to the pool as its own task. A record longer than a
/// chunk is read whole into a chunk of its own, so every chunk ends on a
/// record boundary.
pub fn map_reader_chunks_pipeline<R, T, F>(reader: R, per_chunk: F) -> anyhow::Result<Vec<T>>
where
    R: Read + Send,
    T: Send,
    F: Fn(&[u8]) -> anyhow::Result<T> + Sync,
{
    let results = map_reader_chunks_indexed(reader, per_chunk)?;
    Ok(results.into_iter().map(|(_, result)| result).collect())
}

/// Buffer size of [`map_reader_chunks_pipeline`]; a longer record gets a
/// buffer of its own length.
const CHUNK_SIZE: usize = 16 * 1024 * 1024;
/// Peak memory of [`map_reader_chunks_pipeline`] is
/// `CHUNK_SIZE * BUFFERS_PER_THREAD * threads`, plus any record longer than
/// a chunk.
const BUFFERS_PER_THREAD: usize = 2;

/// [`map_reader_chunks_pipeline`], with each result paired with the position
/// of its chunk in the input; the results are still in arbitrary order.
fn map_reader_chunks_indexed<R, T, F>(reader: R, per_chunk: F) -> anyhow::Result<Vec<(usize, T)>>
where
    R: Read + Send,
    T: Send,
    F: Fn(&[u8]) -> anyhow::Result<T> + Sync,
{
    map_reader_chunks_sized(reader, CHUNK_SIZE, BUFFERS_PER_THREAD, per_chunk)
}

/// A chunk buffer on loan to a task. Returning it on drop rather than at the
/// end of the task keeps an unwind from leaking it, which would eventually
/// wedge the reader in the backpressure loop instead of propagating the panic.
struct Recycled<'a> {
    buf: Option<Vec<u8>>,
    free_tx: &'a std::sync::mpsc::SyncSender<Vec<u8>>,
    chunk_size: usize,
}

impl Recycled<'_> {
    fn bytes(&self) -> &[u8] {
        self.buf.as_deref().expect("buffer is taken only on drop")
    }
}

impl Drop for Recycled<'_> {
    fn drop(&mut self) {
        let Some(buf) = self.buf.take() else {
            return;
        };
        // A grown buffer is replaced by a fresh chunk, so the rotation keeps
        // its size and the reader's backpressure its meaning.
        let returned = if buf.len() == self.chunk_size {
            buf
        } else {
            drop(buf);
            vec![0u8; self.chunk_size]
        };
        let _ = self.free_tx.try_send(returned);
    }
}

/// Each result is paired with the position of its chunk in the input.
fn map_reader_chunks_sized<R, T, F>(
    reader: R,
    chunk_size: usize,
    buffers_per_thread: usize,
    per_chunk: F,
) -> anyhow::Result<Vec<(usize, T)>>
where
    R: Read + Send,
    T: Send,
    F: Fn(&[u8]) -> anyhow::Result<T> + Sync,
{
    let max_buffers = rayon::current_num_threads().max(1) * buffers_per_thread;
    // Holds every buffer that can exist, so handing one back never fails.
    let (free_tx, free_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(max_buffers);
    let mut allocated = 0usize;

    let parsed: Mutex<Vec<(usize, T)>> = Mutex::new(Vec::new());
    let failed: Mutex<Option<anyhow::Error>> = Mutex::new(None);

    // Not `scope`, which would run this blocking read loop on a pool worker
    // the tasks it feeds are waiting for.
    rayon::in_place_scope(|scope| -> anyhow::Result<()> {
        let mut reader = reader;
        let mut leftover: Vec<u8> = Vec::new();
        let (parsed, failed, free_tx, per_chunk) = (&parsed, &failed, &free_tx, &per_chunk);
        let mut chunk_index = 0usize;
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
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
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
                    // No newline in a full buffer: a record longer than a
                    // chunk, or a final record ending on the boundary. Grow
                    // the buffer until the record's newline or EOF.
                    None => loop {
                        let read_from = buf.len();
                        buf.resize(read_from + chunk_size, 0);
                        let mut filled = 0;
                        while read_from + filled < buf.len() {
                            match reader.read(&mut buf[read_from + filled..]) {
                                Ok(0) => break,
                                Ok(n) => filled += n,
                                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                                Err(e) => {
                                    return Err(anyhow::Error::from(e).context(
                                        "Failed to read chunk during pipelined JSON-lines parsing",
                                    ));
                                }
                            }
                        }
                        let end = read_from + filled;
                        if let Some(last_nl) = buf[read_from..end].iter().rposition(|&b| b == b'\n')
                        {
                            let last_nl = read_from + last_nl;
                            leftover.extend_from_slice(&buf[last_nl + 1..end]);
                            valid = last_nl + 1;
                            break;
                        }
                        if end < buf.len() {
                            at_eof = true;
                            valid = end;
                            break;
                        }
                    },
                }
            }

            let index = chunk_index;
            chunk_index += 1;
            scope.spawn(move |_| {
                let buf = Recycled {
                    buf: Some(buf),
                    free_tx,
                    chunk_size,
                };
                match per_chunk(&buf.bytes()[..valid]) {
                    Ok(value) => parsed
                        .lock()
                        .expect("chunk-parse mutex should not be poisoned")
                        .push((index, value)),
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

/// Records one rayon job of [`write_json_lines_parallel`] renders as a unit,
/// at most; a small input is split finer so the whole pool shares it.
const MAX_RECORDS_PER_RENDER_BLOCK: usize = 1024;
const MIN_RECORDS_PER_RENDER_BLOCK: usize = 32;
/// Blocks each thread renders per wave, and the most blocks a wave holds,
/// which bounds the rendered bytes in memory to a wave on a many-core host.
const RENDER_BLOCKS_PER_THREAD: usize = 2;
const MAX_RENDER_BLOCKS_PER_WAVE: usize = 256;

/// [`write_json_lines`] for a slice: blocks of records are rendered on the
/// rayon pool, a wave of blocks at a time, and written in order, so the
/// output is the same bytes as the sequential version.
pub fn write_json_lines_parallel<W: Write, T: Serialize + Sync>(
    out: W,
    xs: &[T],
) -> anyhow::Result<()> {
    let mut writer = BufWriter::with_capacity(BUFFER_SIZE, out);
    write_json_lines_parallel_into(&mut writer, xs)?;
    writer.flush()?;
    Ok(())
}

/// [`write_json_lines_parallel`] into a writer the caller buffers and
/// flushes, for a caller that writes many batches to one output.
pub fn write_json_lines_parallel_into<W: Write, T: Serialize + Sync>(
    writer: &mut W,
    xs: &[T],
) -> anyhow::Result<()> {
    let blocks_per_wave = (rayon::current_num_threads().max(1) * RENDER_BLOCKS_PER_THREAD)
        .min(MAX_RENDER_BLOCKS_PER_WAVE);
    let records_per_block = xs
        .len()
        .div_ceil(blocks_per_wave)
        .clamp(MIN_RECORDS_PER_RENDER_BLOCK, MAX_RECORDS_PER_RENDER_BLOCK);
    let blocks: Vec<&[T]> = xs.chunks(records_per_block).collect();
    for wave in blocks.chunks(blocks_per_wave) {
        let rendered = wave
            .par_iter()
            .map(|block| render_json_lines(block))
            .collect::<anyhow::Result<Vec<Vec<u8>>>>()?;
        for bytes in &rendered {
            writer.write_all(bytes)?;
        }
    }
    Ok(())
}

fn render_json_lines<T: Serialize>(records: &[T]) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, record)?;
        bytes.push(b'\n');
    }
    Ok(bytes)
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
    use std::io::Read;
    use std::io::Write;
    use std::io::{self};

    use tempfile::NamedTempFile;

    use crate::json::json_lines;
    use crate::json::map_reader_chunks_sized;
    use crate::json::read_file_lines;
    use crate::json::read_file_lines_parallel;
    use crate::json::read_file_lines_parallel_ordered;
    use crate::json::read_reader_lines_chunked_pipeline_ordered;
    use crate::json::write_json_lines;
    use crate::json::write_json_lines_parallel;
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

        // The parallel writer produces the sequential writer's bytes.
        let mut sequential = Vec::new();
        write_json_lines(&mut sequential, &data).unwrap();
        let mut parallel = Vec::new();
        write_json_lines_parallel(&mut parallel, &data).unwrap();
        assert_eq!(parallel, sequential);
        let mut empty = Vec::new();
        write_json_lines_parallel(&mut empty, &data[..0]).unwrap();
        assert!(empty.is_empty());
    }

    /// The ordered reader keeps the input order across chunk seams, and the
    /// parallel writer writes blocks and waves in order.
    #[test]
    fn ordered_reading_and_parallel_writing_keep_the_input_order() {
        let data: Vec<u64> = (0..200_000).collect();
        let mut written = Vec::new();
        write_json_lines_parallel(&mut written, &data).unwrap();
        assert_eq!(written.iter().filter(|&&b| b == b'\n').count(), data.len());

        let read: Vec<u64> = map_reader_chunks_sized(written.as_slice(), 64, 1, |chunk| {
            json_lines(chunk)
                .map(|line| anyhow::Ok(serde_json::from_slice::<u64>(line)?))
                .collect::<anyhow::Result<Vec<u64>>>()
        })
        .map(|mut chunks| {
            chunks.sort_unstable_by_key(|(index, _)| *index);
            chunks.into_iter().flat_map(|(_, chunk)| chunk).collect()
        })
        .unwrap();
        assert_eq!(read, data);

        let read: Vec<u64> =
            read_reader_lines_chunked_pipeline_ordered(written.as_slice()).unwrap();
        assert_eq!(read, data);
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
        assert_eq!(counted.iter().map(|(_, n)| n).sum::<usize>(), 20_000);
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
        .flat_map(|(_, chunk)| chunk)
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
        assert_eq!(counted.iter().map(|(_, n)| n).sum::<usize>(), 0);
    }

    #[test]
    fn final_record_without_trailing_newline_is_read() {
        let counted = map_reader_chunks_sized(&b"{\"n\":1}\n{\"n\":2}"[..], 64, 1, |chunk| {
            anyhow::Ok(json_lines(chunk).count())
        })
        .unwrap();
        assert_eq!(counted.iter().map(|(_, n)| n).sum::<usize>(), 2);
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
        assert_eq!(counted.iter().map(|(_, n)| n).sum::<usize>(), 2);
    }

    #[test]
    fn unterminated_final_record_filling_the_chunk_exactly_is_read() {
        let record = format!("{{\"n\":\"{}\"}}", "x".repeat(56));
        assert_eq!(record.len(), 64);
        let counted = map_reader_chunks_sized(record.as_bytes(), 64, 1, |chunk| {
            anyhow::Ok(json_lines(chunk).count())
        })
        .unwrap();
        assert_eq!(counted.iter().map(|(_, n)| n).sum::<usize>(), 1);
    }

    /// Every record of `input`, read through 64-byte chunks, as
    /// `(chunk index, record)` pairs in chunk order.
    fn read_in_small_chunks(input: &[u8]) -> Vec<(usize, String)> {
        let mut chunks = map_reader_chunks_sized(input, 64, 1, |chunk| {
            anyhow::Ok(
                json_lines(chunk)
                    .map(|line| String::from_utf8(line.to_vec()).unwrap())
                    .collect::<Vec<String>>(),
            )
        })
        .unwrap();
        chunks.sort_unstable_by_key(|(index, _)| *index);
        chunks
            .into_iter()
            .flat_map(|(index, records)| records.into_iter().map(move |record| (index, record)))
            .collect()
    }

    #[test]
    fn records_longer_than_a_chunk_are_read_whole() {
        let records = |n: usize| format!("{{\"n\":\"{}\"}}", "x".repeat(n));
        let exactly_a_chunk = records(56);
        assert_eq!(exactly_a_chunk.len(), 64);
        let longer = records(92);
        let much_longer = records(500);
        let tail: Vec<String> = (0..50).map(|i| format!("{{\"n\":{i}}}")).collect();
        let mut expected = vec![exactly_a_chunk.clone(), longer.clone(), much_longer.clone()];
        expected.extend(tail.iter().cloned());

        // Terminated, and ending on the boundary of the first chunk.
        let input = format!("{}\n", expected.join("\n"));
        let read = read_in_small_chunks(input.as_bytes());
        assert_eq!(
            read.iter().map(|(_, r)| r.clone()).collect::<Vec<_>>(),
            expected
        );
        // The long records each got a chunk of their own.
        assert_ne!(read[0].0, read[1].0);
        assert_ne!(read[1].0, read[2].0);

        // The same records after a short line, so the long ones straddle
        // chunk boundaries differently.
        let input = format!("{{\"n\":1}}\n{}\n", expected.join("\n"));
        let read: Vec<String> = read_in_small_chunks(input.as_bytes())
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        assert_eq!(read[1..], expected[..]);

        // An unterminated final record longer than a chunk.
        let input = format!("{{\"n\":1}}\n{much_longer}");
        let read: Vec<String> = read_in_small_chunks(input.as_bytes())
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        assert_eq!(read, vec!["{\"n\":1}".to_owned(), much_longer]);
    }

    /// A reader that reports `Interrupted` before every read.
    struct Interrupting<'a> {
        data: &'a [u8],
        interrupt_next: bool,
    }

    impl Read for Interrupting<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if std::mem::replace(&mut self.interrupt_next, false) {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            self.interrupt_next = true;
            let n = self.data.len().min(buf.len()).min(7);
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    #[test]
    fn interrupted_reads_are_retried() {
        let input: Vec<u8> = (0..2_000)
            .flat_map(|i| {
                if i % 50 == 0 {
                    format!("{{\"n\":{i},\"pad\":\"{}\"}}\n", "y".repeat(150)).into_bytes()
                } else {
                    format!("{{\"n\":{i}}}\n").into_bytes()
                }
            })
            .collect();
        let reader = Interrupting {
            data: &input,
            interrupt_next: true,
        };
        let counted =
            map_reader_chunks_sized(reader, 64, 1, |chunk| anyhow::Ok(json_lines(chunk).count()))
                .unwrap();
        assert_eq!(counted.iter().map(|(_, n)| n).sum::<usize>(), 2_000);
    }

    #[test]
    fn long_records_under_backpressure_keep_the_reader_going() {
        let input: Vec<u8> = (0..20_000)
            .flat_map(|i| {
                if i % 100 == 0 {
                    format!("{{\"n\":{i},\"pad\":\"{}\"}}\n", "y".repeat(200)).into_bytes()
                } else {
                    format!("{{\"n\":{i}}}\n").into_bytes()
                }
            })
            .collect();
        let counted = map_reader_chunks_sized(input.as_slice(), 64, 1, |chunk| {
            anyhow::Ok(json_lines(chunk).count())
        })
        .unwrap();
        assert_eq!(counted.iter().map(|(_, n)| n).sum::<usize>(), 20_000);
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
            .map(|(_, n)| n)
            .sum::<usize>()
        };
        let (a, b) = pool.install(|| rayon::join(read, read));
        assert_eq!((a, b), (20_000, 20_000));
    }
}
