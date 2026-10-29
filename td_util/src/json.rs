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

/// Read JSON lines from a reader in chunks and parse each chunk on the rayon
/// pool, so reading (and decompression) overlaps with parsing.
///
/// Compared to reading the whole input before parsing, this:
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

/// The most records in a block, which one rayon job of [`render_in_order`]
/// renders; a small input gets smaller blocks so that the whole pool shares it.
pub const MAX_RECORDS_PER_BLOCK: usize = 1024;
/// The fewest records in a block, except the last block of a group.
const MIN_RECORDS_PER_BLOCK: usize = 32;
/// Blocks that each pool thread renders per wave.
const BLOCKS_PER_THREAD: usize = 2;
/// The most blocks in a wave, whatever the pool size.
pub const MAX_BLOCKS_PER_WAVE: usize = 256;
/// The most records that [`render_in_order`] holds rendered at once: two
/// waves of the largest blocks.
pub const MAX_RENDERED_RECORDS: usize = 2 * MAX_BLOCKS_PER_WAVE * MAX_RECORDS_PER_BLOCK;

/// How [`render_in_order`] splits the records into parallel work.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
struct RenderShape {
    records_per_block: usize,
    blocks_per_wave: usize,
}

impl RenderShape {
    /// The shape for `records` records on a pool of `threads` threads.
    fn for_records(records: usize, threads: usize) -> Self {
        let blocks_per_wave = (threads.max(1) * BLOCKS_PER_THREAD).min(MAX_BLOCKS_PER_WAVE);
        Self {
            records_per_block: records
                .div_ceil(blocks_per_wave)
                .clamp(MIN_RECORDS_PER_BLOCK, MAX_RECORDS_PER_BLOCK),
            blocks_per_wave,
        }
    }
}

/// Renders `groups` of records on the rayon pool and passes each rendered
/// block to `write` in record order. A block holds consecutive records of
/// one group, and `render` gets the index of that group. `write` runs on the
/// calling thread and consumes one wave of blocks while the pool renders the
/// next wave; the two overlap only when a pool thread other than the calling
/// one is free. At most two waves of rendered blocks, and at most
/// [`MAX_RENDERED_RECORDS`] records, are in memory.
///
/// The calling thread waits for each render while it holds what `write`
/// holds, so `write` must not hold a lock that `render` needs, such as the
/// stdout lock when rendering prints.
///
/// An error ends the call once no render that it started is still running,
/// and the output then holds only part of the records. After a render error,
/// `write` still gets every block of the waves before the failed one; after a
/// write error, `write` gets no further block. The error is the one that
/// `render` or `write` returned. A panic in `render` or `write` reaches the
/// caller after the same wait, and the output then also holds part of the
/// records.
pub fn render_in_order<G, T, R, E, F, W>(groups: &[G], render: F, write: W) -> Result<(), E>
where
    G: AsRef<[T]>,
    T: Sync,
    R: Send,
    E: Send,
    F: Fn(usize, &[T]) -> Result<R, E> + Sync,
    W: FnMut(R) -> Result<(), E>,
{
    let records = groups.iter().map(|group| group.as_ref().len()).sum();
    let shape = RenderShape::for_records(records, rayon::current_num_threads());
    render_in_order_with_shape(groups, shape, render, write)
}

fn render_in_order_with_shape<G, T, R, E, F, W>(
    groups: &[G],
    shape: RenderShape,
    render: F,
    mut write: W,
) -> Result<(), E>
where
    G: AsRef<[T]>,
    T: Sync,
    R: Send,
    E: Send,
    F: Fn(usize, &[T]) -> Result<R, E> + Sync,
    W: FnMut(R) -> Result<(), E>,
{
    let blocks: Vec<(usize, &[T])> = groups
        .iter()
        .enumerate()
        .flat_map(|(group, records)| {
            records
                .as_ref()
                .chunks(shape.records_per_block)
                .map(move |block| (group, block))
        })
        .collect();

    let mut rendered = Vec::new();
    for wave in blocks.chunks(shape.blocks_per_wave) {
        // `Scope::spawn` returns nothing, and `rayon::join` would need a
        // `Send` write, so the render hands its result back through `next`.
        let mut next = None;
        rayon::in_place_scope(|scope| {
            scope.spawn(|_| {
                next = Some(
                    wave.par_iter()
                        .map(|&(group, block)| render(group, block))
                        .collect::<Result<Vec<R>, E>>(),
                );
            });
            rendered.drain(..).try_for_each(&mut write)
        })?;
        rendered = next.expect("should be set: the scope waits for the render it spawned")?;
    }
    rendered.into_iter().try_for_each(write)
}

/// [`write_json_lines`] for a slice: blocks of records are rendered on the
/// rayon pool by [`render_in_order`] and written in order, so the output is
/// the same bytes as the sequential version.
pub fn write_json_lines_parallel<W: Write, T: Serialize + Sync>(
    out: W,
    xs: &[T],
) -> anyhow::Result<()> {
    let mut writer = BufWriter::with_capacity(BUFFER_SIZE, out);
    write_json_lines_parallel_into(&mut writer, xs)?;
    writer.flush().context("flushing JSON lines")
}

/// [`write_json_lines_parallel`] into a writer the caller buffers and
/// flushes, for a caller that writes many batches to one output.
pub fn write_json_lines_parallel_into<W: Write, T: Serialize + Sync>(
    writer: &mut W,
    xs: &[T],
) -> anyhow::Result<()> {
    render_in_order(
        &[xs],
        |_, records| render_json_lines(records).context("rendering JSON lines"),
        |bytes| writer.write_all(&bytes).context("writing JSON lines"),
    )
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
    use std::collections::HashMap;
    use std::io::Read;
    use std::io::Write;
    use std::io::{self};
    use std::panic::catch_unwind;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use anyhow::Context;
    use rayon::ThreadPool;
    use rayon::ThreadPoolBuilder;
    use rstest::rstest;
    use tempfile::NamedTempFile;

    use crate::json::BLOCKS_PER_THREAD;
    use crate::json::MAX_RECORDS_PER_BLOCK;
    use crate::json::RenderShape;
    use crate::json::json_lines;
    use crate::json::map_reader_chunks_sized;
    use crate::json::read_file_lines;
    use crate::json::read_file_lines_parallel;
    use crate::json::read_file_lines_parallel_ordered;
    use crate::json::read_reader_lines_chunked_pipeline_ordered;
    use crate::json::render_in_order_with_shape;
    use crate::json::write_json_lines;
    use crate::json::write_json_lines_parallel;
    use crate::json::write_json_lines_parallel_into;
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

    /// On a small pool, the output spans several waves and is still the same
    /// bytes as the sequential writer.
    #[test]
    fn parallel_writing_across_waves_matches_sequential_writing() {
        /// Two full waves and one more block of the largest blocks on a pool
        /// of [`TEST_THREADS`] threads, so that the output spans three waves.
        const RECORDS: usize = (2 * TEST_THREADS * BLOCKS_PER_THREAD + 1) * MAX_RECORDS_PER_BLOCK;
        let data: Vec<usize> = (0..RECORDS).collect();
        let mut sequential = Vec::new();
        write_json_lines(&mut sequential, &data).unwrap();

        let mut parallel = Vec::new();
        thread_pool(TEST_THREADS)
            .install(|| write_json_lines_parallel_into(&mut parallel, &data))
            .unwrap();

        assert_eq!(
            parallel, sequential,
            "the same bytes as the sequential writer"
        );
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
        let panicked = catch_unwind(|| {
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
        let pool = thread_pool(TEST_THREADS);
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

    /// Time after which a test that waits for another rayon task fails.
    const WAIT_DEADLINE: Duration = Duration::from_secs(30);

    /// One record per block and two blocks per wave, so that a few records
    /// span several waves.
    const ONE_RECORD_BLOCKS: RenderShape = RenderShape {
        records_per_block: 1,
        blocks_per_wave: 2,
    };

    /// Records that fill four waves of [`ONE_RECORD_BLOCKS`].
    const FOUR_WAVES_OF_RECORDS: usize = 8;

    /// Threads of a small test pool: the fewest that let one task wait for
    /// another.
    const TEST_THREADS: usize = 2;

    /// A pool of `threads` threads.
    fn thread_pool(threads: usize) -> ThreadPool {
        ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
    }

    /// Waits for a signal from another task, and fails with `what` when none
    /// arrives within `WAIT_DEADLINE`.
    fn wait_for(signal: &Mutex<mpsc::Receiver<()>>, what: &'static str) -> anyhow::Result<()> {
        signal
            .lock()
            .unwrap()
            .recv_timeout(WAIT_DEADLINE)
            .context(what)
    }

    /// Every shape writes the records in their order, fills each block of a
    /// group up to the block size except the last one, and tells `render` the
    /// group of every block.
    #[rstest]
    #[case::one_record_per_block(1, 1)]
    #[case::short_last_blocks(3, 2)]
    #[case::first_group_fills_one_block(7, 1)]
    #[case::blocks_larger_than_small_groups(8, 3)]
    fn render_in_order_keeps_record_order_for_every_shape(
        #[case] records_per_block: usize,
        #[case] blocks_per_wave: usize,
    ) {
        let groups: Vec<Vec<u32>> = vec![
            (0..7).collect(),
            Vec::new(),
            (7..30).collect(),
            (30..31).collect(),
        ];
        let shape = RenderShape {
            records_per_block,
            blocks_per_wave,
        };
        let mut written = Vec::new();
        let mut block_lengths = Vec::new();

        render_in_order_with_shape(
            &groups,
            shape,
            |group, block| {
                anyhow::Ok(
                    block
                        .iter()
                        .map(|&record| (group, record))
                        .collect::<Vec<_>>(),
                )
            },
            |block| {
                block_lengths.push((block[0].0, block.len()));
                written.extend(block);
                Ok(())
            },
        )
        .unwrap();

        let expected: Vec<(usize, u32)> = groups
            .iter()
            .enumerate()
            .flat_map(|(group, records)| records.iter().map(move |&record| (group, record)))
            .collect();
        assert_eq!(written, expected, "every record with its group, in order");
        let expected_lengths: Vec<(usize, usize)> = groups
            .iter()
            .enumerate()
            .flat_map(|(group, records)| {
                (0..records.len())
                    .step_by(records_per_block)
                    .map(move |start| (group, records_per_block.min(records.len() - start)))
            })
            .collect();
        assert_eq!(
            block_lengths, expected_lengths,
            "full blocks, then a shorter last block per group"
        );
    }

    /// The block size is [`super::MIN_RECORDS_PER_BLOCK`] to
    /// [`MAX_RECORDS_PER_BLOCK`] records: the total record count divided over
    /// one wave, where that fits. A wave holds [`BLOCKS_PER_THREAD`] blocks per
    /// thread, at most [`super::MAX_BLOCKS_PER_WAVE`].
    #[rstest]
    #[case::empty_output(0, 1, 32, 2)]
    #[case::zero_threads_count_as_one(100, 0, 50, 2)]
    #[case::small_output_gets_the_smallest_blocks(100, 4, 32, 8)]
    #[case::output_fills_one_wave(2_000, 4, 250, 8)]
    #[case::large_output_gets_the_largest_blocks(100_000, 4, 1024, 8)]
    #[case::many_threads_reach_the_wave_cap(100_000, 200, 391, 256)]
    fn render_shape_follows_the_records_and_the_threads(
        #[case] records: usize,
        #[case] threads: usize,
        #[case] records_per_block: usize,
        #[case] blocks_per_wave: usize,
    ) {
        assert_eq!(
            RenderShape::for_records(records, threads),
            RenderShape {
                records_per_block,
                blocks_per_wave,
            },
            "{records} records on {threads} threads"
        );
    }

    /// Where the thread that calls `render_in_order` runs.
    #[derive(Clone, Copy, Debug)]
    enum Caller {
        PoolWorker,
        /// For example, a program's main thread.
        OutsidePool,
    }

    /// The pool renders the next wave while the calling thread writes the
    /// current one: the first write and the first render of the second wave
    /// each wait for the other, so a loop that renders and writes in turn
    /// times out. No more than two waves of rendered blocks are held.
    #[rstest]
    #[case::pool_worker(Caller::PoolWorker)]
    #[case::outside_pool(Caller::OutsidePool)]
    fn rendering_overlaps_writing_and_holds_at_most_two_waves(#[case] caller: Caller) {
        let shape = ONE_RECORD_BLOCKS;
        let records: Vec<usize> = (0..FOUR_WAVES_OF_RECORDS).collect();
        // With one record per block, this record starts the second wave.
        let second_wave_start = shape.blocks_per_wave;
        let held = AtomicUsize::new(0);
        let most_held = AtomicUsize::new(0);
        let (writing_started, writing_seen) = mpsc::channel();
        let writing_seen = Mutex::new(writing_seen);
        let (second_wave_started, second_wave_seen) = mpsc::channel();
        let second_wave_seen = Mutex::new(second_wave_seen);
        let mut written = Vec::new();

        let mut run = || {
            render_in_order_with_shape(
                &[&records],
                shape,
                |_, block| {
                    let now_held = held.fetch_add(1, Ordering::SeqCst) + 1;
                    most_held.fetch_max(now_held, Ordering::SeqCst);
                    if block[0] == second_wave_start {
                        wait_for(
                            &writing_seen,
                            "the first wave was not written while the second rendered",
                        )?;
                        second_wave_started.send(()).unwrap();
                    }
                    anyhow::Ok(block[0])
                },
                |record| {
                    if record == 0 {
                        writing_started.send(()).unwrap();
                        wait_for(
                            &second_wave_seen,
                            "the second wave did not render while the first was written",
                        )?;
                    }
                    held.fetch_sub(1, Ordering::SeqCst);
                    written.push(record);
                    Ok(())
                },
            )
        };
        match caller {
            Caller::PoolWorker => thread_pool(TEST_THREADS).install(run),
            Caller::OutsidePool => run(),
        }
        .unwrap();

        assert_eq!(written, records, "every record, in record order");
        let most_held = most_held.load(Ordering::SeqCst);
        assert!(
            most_held <= 2 * shape.blocks_per_wave,
            "{most_held} rendered blocks were held at once"
        );
    }

    /// A panic in `render` reaches the caller.
    #[test]
    fn a_render_panic_reaches_the_caller() {
        /// The record whose render panics.
        const PANIC_AT: usize = 5;
        let records: Vec<usize> = (0..FOUR_WAVES_OF_RECORDS).collect();
        let shape = ONE_RECORD_BLOCKS;

        let panicked = catch_unwind(|| {
            render_in_order_with_shape(
                &[&records],
                shape,
                |_, block| {
                    if block[0] == PANIC_AT {
                        panic!("render panics at {PANIC_AT}");
                    }
                    anyhow::Ok(block[0])
                },
                |_| Ok(()),
            )
        });

        let payload = panicked.expect_err("the render panic should reach the caller");
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied());
        assert_eq!(
            message,
            Some(format!("render panics at {PANIC_AT}").as_str()),
            "the caller should get the payload of the render panic"
        );
    }

    /// Two calls that run at once on a full pool both complete: a call never
    /// blocks a pool thread on a render that no free thread can run.
    #[test]
    fn concurrent_renders_on_a_full_pool_complete() {
        /// Records for 32 waves of [`ONE_RECORD_BLOCKS`], so that both calls
        /// hand over many waves.
        const RECORDS: usize = 64;
        let (done, finished) = mpsc::channel();
        // A detached thread, so that a deadlock fails the test after
        // `WAIT_DEADLINE` instead of hanging it.
        thread::spawn(move || {
            let records: Vec<usize> = (0..RECORDS).collect();
            let render = || {
                let mut written = Vec::new();
                render_in_order_with_shape(
                    &[&records],
                    ONE_RECORD_BLOCKS,
                    |_, block| anyhow::Ok(block[0]),
                    |record| {
                        written.push(record);
                        Ok(())
                    },
                )
                .map(|()| written)
            };
            // The receiver is gone when the test has already timed out.
            let _ = done.send(thread_pool(TEST_THREADS).install(|| rayon::join(render, render)));
        });

        let (first, second) = finished
            .recv_timeout(WAIT_DEADLINE)
            .expect("should complete: two calls on a full pool must not deadlock");

        let records: Vec<usize> = (0..RECORDS).collect();
        assert_eq!(
            first.unwrap(),
            records,
            "the first call writes every record, in order"
        );
        assert_eq!(
            second.unwrap(),
            records,
            "the second call writes every record, in order"
        );
    }

    /// On a one-thread pool, the calling worker writes each wave and then
    /// renders the next one itself, so the output is still complete and in
    /// order.
    #[test]
    fn render_in_order_completes_on_a_one_thread_pool() {
        /// Records for five waves of [`ONE_RECORD_BLOCKS`].
        const RECORDS: usize = 10;
        let records: Vec<usize> = (0..RECORDS).collect();
        let shape = ONE_RECORD_BLOCKS;
        let mut written = Vec::new();

        thread_pool(1)
            .install(|| {
                render_in_order_with_shape(
                    &[&records],
                    shape,
                    |_, block| anyhow::Ok(block[0]),
                    |record| {
                        written.push(record);
                        Ok(())
                    },
                )
            })
            .unwrap();

        assert_eq!(written, records, "every record, in record order");
    }

    /// Blocks of one wave that finish rendering out of order are still
    /// written in record order.
    #[test]
    fn blocks_rendered_out_of_order_are_written_in_order() {
        let records: Vec<usize> = vec![0, 1];
        let shape = ONE_RECORD_BLOCKS;
        let (second_rendered, second_seen) = mpsc::channel();
        let second_seen = Mutex::new(second_seen);
        let mut written = Vec::new();

        thread_pool(TEST_THREADS)
            .install(|| {
                render_in_order_with_shape(
                    &[&records],
                    shape,
                    |_, block| {
                        if block[0] == 0 {
                            wait_for(&second_seen, "the second block did not render first")?;
                        } else {
                            second_rendered.send(()).unwrap();
                        }
                        anyhow::Ok(block[0])
                    },
                    |record| {
                        written.push(record);
                        Ok(())
                    },
                )
            })
            .unwrap();

        assert_eq!(written, records, "record order, not render order");
    }

    /// The step at which a call fails, and the record that it fails at.
    #[derive(Clone, Copy, Debug)]
    enum FailAt {
        Render(usize),
        Write(usize),
    }

    /// A render error ends the output after the waves before it, and a write
    /// error ends it at once; the caller gets either error unchanged.
    #[rstest]
    #[case::render_error(FailAt::Render(5), "render failed at 5", &[0, 1, 2, 3])]
    #[case::write_error(FailAt::Write(3), "write failed at 3", &[0, 1, 2])]
    fn render_and_write_errors_reach_the_caller(
        #[case] fail_at: FailAt,
        #[case] expected_error: &str,
        #[case] expected_written: &[usize],
    ) {
        let records: Vec<usize> = (0..FOUR_WAVES_OF_RECORDS).collect();
        let shape = ONE_RECORD_BLOCKS;
        let mut written = Vec::new();

        let error = render_in_order_with_shape(
            &[&records],
            shape,
            |_, block| {
                anyhow::ensure!(
                    !matches!(fail_at, FailAt::Render(at) if at == block[0]),
                    "render failed at {}",
                    block[0]
                );
                Ok(block[0])
            },
            |record| {
                anyhow::ensure!(
                    !matches!(fail_at, FailAt::Write(at) if at == record),
                    "write failed at {record}"
                );
                written.push(record);
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            expected_error,
            "the caller should get the error unchanged"
        );
        assert_eq!(
            written, expected_written,
            "after a render error, the waves before the failed one; after a write error, the records before it"
        );
    }

    /// A writer whose every write fails, as on a full disk.
    struct FullDisk;

    impl Write for FullDisk {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The step of a JSON-lines write that fails.
    #[derive(Clone, Copy, Debug)]
    enum Step {
        Render,
        Write,
        Flush,
    }

    /// A JSON-lines error names the step that failed.
    #[rstest]
    #[case::render(Step::Render, "rendering JSON lines")]
    #[case::write(Step::Write, "writing JSON lines")]
    #[case::flush(Step::Flush, "flushing JSON lines")]
    fn json_lines_errors_name_the_failed_step(#[case] step: Step, #[case] expected: &str) {
        let error = match step {
            Step::Render => {
                let record: HashMap<(u32, u32), u32> = HashMap::from([((0, 0), 0)]);
                write_json_lines_parallel_into(&mut Vec::new(), &[record])
            }
            Step::Write => write_json_lines_parallel_into(&mut FullDisk, &[1_u32]),
            // `write_json_lines_parallel` buffers a small output, so the
            // write reaches `FullDisk` only at the flush.
            Step::Flush => write_json_lines_parallel(FullDisk, &[1_u32]),
        }
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            expected,
            "the outermost context should name the step that failed"
        );
    }
}
