/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Stdout;
use std::io::Write;
use std::path::Path;
use std::thread;

use anyhow::Context;

use crate::json::BUFFER_SIZE;
use crate::zstd::has_zstd_magic;
use crate::zstd::is_zstd;

/// Creates or truncates `file_path` and opens it for writing; a `.zst` path
/// is compressed as one zstd frame by the writing thread. The same as
/// [`file_writer_with_zstd_threads`] with [`ZstdThreads::Writer`]. Call
/// [`FileWriter::finish`] after the last write.
pub fn file_writer(file_path: &Path) -> anyhow::Result<FileWriter> {
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
/// changes only the compressed bytes, not what a reader decompresses. Call
/// [`FileWriter::finish`] after the last write.
pub fn file_writer_with_zstd_threads(
    file_path: &Path,
    threads: ZstdThreads,
) -> anyhow::Result<FileWriter> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(file_path)
        .with_context(|| format!("Unable to open file `{}` for writing", file_path.display()))?;

    let output = if is_zstd(file_path) {
        let mut encoder = zstd::Encoder::new(file, zstd::DEFAULT_COMPRESSION_LEVEL)?;
        let workers = threads.zstd_workers();
        // A zstd without worker support still compresses correctly.
        let compresses_on_workers = workers > 0
            && encoder
                .multithread(workers)
                .inspect_err(|error| {
                    tracing::warn!(
                        "zstd cannot compress `{}` on {workers} threads, using one: {error}",
                        file_path.display()
                    )
                })
                .is_ok();
        // The worker encoder copies its input into job buffers, so a large
        // block goes to it directly: a `BufWriter` passes through any write
        // at least its own size, and a small buffer only gathers smaller
        // writes.
        let buffer_size = if compresses_on_workers {
            ZSTD_WORKER_WRITE_BUFFER_SIZE
        } else {
            BUFFER_SIZE
        };
        Output::Zstd(BufWriter::with_capacity(buffer_size, encoder))
    } else {
        Output::Plain(BufWriter::with_capacity(BUFFER_SIZE, file))
    };
    Ok(FileWriter {
        name: format!("`{}`", file_path.display()),
        created_during_panic: thread::panicking(),
        output: Some(output),
    })
}

/// A file or a zstd-compressed file from [`file_writer`] or
/// [`file_writer_with_zstd_threads`], or standard output from
/// [`FileWriter::stdout`]. [`FileWriter::finish`] writes the buffered
/// bytes and the end of the zstd frame, and returns the first error. A writer
/// dropped without `finish`, also after a failed write, still finishes, but it
/// can only log an error.
///
/// After a failed finish, or when a panic that started while it was open
/// drops it, a writer of a file discards its buffered bytes and writes nothing
/// more: a plain output can end inside a record, and a `.zst` output has no
/// frame end, so a reader rejects it. Standard output keeps the buffering of
/// [`io::Stdout`].
pub struct FileWriter {
    /// The output's name in errors and logs: a quoted path, or
    /// `standard output`.
    name: String,
    /// Whether a panic was unwinding when the writer was created.
    created_during_panic: bool,
    /// `None` once the output is finished.
    output: Option<Output>,
}

enum Output {
    Plain(BufWriter<File>),
    Zstd(BufWriter<zstd::Encoder<'static, File>>),
    Stdout(Stdout),
}

impl FileWriter {
    /// A writer of standard output, for a tool whose output file is optional.
    pub fn stdout() -> Self {
        Self {
            name: "standard output".to_owned(),
            created_during_panic: thread::panicking(),
            output: Some(Output::Stdout(io::stdout())),
        }
    }

    /// Writes the buffered bytes and, for a `.zst` output, the end of the zstd
    /// frame, and returns the first error from those writes. It does not sync
    /// the file to disk, so it does not see an error that only closing the file
    /// reports. A writer dropped without `finish` also ends its frame, unless
    /// a panic that started while it was open drops it, so a readable output
    /// does not show that every write succeeded.
    pub fn finish(mut self) -> anyhow::Result<()> {
        let output = self
            .output
            .take()
            .expect("should hold its output until `finish`");
        output
            .finish()
            .with_context(|| format!("finishing {}", self.name))
    }

    fn output(&mut self) -> &mut dyn Write {
        match self
            .output
            .as_mut()
            .expect("should hold its output until `finish` or `drop`")
        {
            Output::Plain(writer) => writer,
            Output::Zstd(writer) => writer,
            Output::Stdout(writer) => writer,
        }
    }
}

impl Output {
    fn finish(self) -> io::Result<()> {
        match self {
            Self::Plain(writer) => finish_buffered(writer).map(drop),
            Self::Zstd(writer) => finish_zstd(writer),
            Self::Stdout(mut writer) => writer.flush(),
        }
    }

    /// Drops the output without writing its buffered bytes, and returns
    /// whether it had a buffer. Standard output keeps the buffer of
    /// [`io::Stdout`].
    fn discard(self) -> bool {
        match self {
            Self::Plain(writer) => drop(writer.into_parts()),
            Self::Zstd(writer) => drop(writer.into_parts()),
            Self::Stdout(_) => return false,
        }
        true
    }
}

/// Writes the buffered bytes and the end of the zstd frame.
fn finish_zstd<W: Write>(writer: BufWriter<zstd::Encoder<'_, W>>) -> io::Result<()> {
    finish_buffered(writer)?.finish().map(drop)
}

/// Writes the buffered bytes and returns the inner writer. After a failed
/// write, it drops the buffer without the second write that `BufWriter`'s own
/// drop makes.
fn finish_buffered<W: Write>(writer: BufWriter<W>) -> io::Result<W> {
    writer.into_inner().map_err(|error| {
        let (error, writer) = error.into_parts();
        drop(writer.into_parts());
        error
    })
}

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output().write(buf)
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.output().write_vectored(bufs)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.output().write_all(buf)
    }

    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> io::Result<()> {
        self.output().write_fmt(args)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output().flush()
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        let Some(output) = self.output.take() else {
            return;
        };
        // A panic that started while the output was open can leave it in any
        // state, so it is not finished. std has no public panic count, so the
        // check uses this thread's panic state: a writer that was created
        // during one panic and outlives it, or that moved to another thread,
        // can still finish when a later panic drops it. A runtime that catches
        // the panic first and drops the writer later, as a spawned task does,
        // sees no panic, so the writer finishes.
        if thread::panicking() && !self.created_during_panic {
            if output.discard() {
                tracing::warn!(
                    "{} was dropped during a panic, so it is not finished",
                    self.name
                );
            }
            return;
        }
        if let Err(error) = output.finish() {
            tracing::error!(
                "{} was dropped before `finish`, and finishing it failed: {error}",
                self.name
            );
        }
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
    use std::cell::Cell;
    use std::fs;
    use std::io::Read;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::symlink;
    use std::panic;
    use std::rc::Rc;

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

    /// `write!` reaches a compressed output.
    #[test]
    pub fn test_formatted_writes_reach_the_output() {
        let out_dir = TempDir::new().unwrap();
        let out_path = out_dir.path().join("formatted.jsonl.zst");

        let mut writer = file_writer(&out_path).unwrap();
        write!(writer, "{DATA} {}", 1).unwrap();
        writer.finish().unwrap();

        let mut decompressed = String::new();
        file_reader(&out_path)
            .unwrap()
            .read_to_string(&mut decompressed)
            .unwrap();
        assert_eq!(
            decompressed,
            format!("{DATA} 1"),
            "the formatted bytes reach the compressed output"
        );
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
            writer.finish()
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

    /// `finish` writes a complete zstd frame.
    #[test]
    pub fn test_round_trip_write_read_compressed() {
        let out_dir = TempDir::new().unwrap();
        let out_path = out_dir.path().join("test_roundtrip.zst");

        let mut writer = file_writer(&out_path).unwrap();
        writer.write_all(DATA.as_bytes()).unwrap();
        writer.finish().unwrap();

        // Read compressed data back
        let mut reader = file_reader(&out_path).unwrap();
        let mut decompressed = String::new();
        std::io::Read::read_to_string(&mut reader, &mut decompressed).unwrap();

        assert_eq!(decompressed, DATA);
    }

    /// Accepts writes until `fail` is set, then reports a full disk. It counts
    /// the writes that fail.
    #[derive(Default)]
    struct FailWhenSet {
        fail: bool,
        failed_writes: Rc<Cell<usize>>,
    }

    impl Write for FailWhenSet {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail {
                self.failed_writes.set(self.failed_writes.get() + 1);
                return Err(io::ErrorKind::StorageFull.into());
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A failure to write the end of the zstd frame reaches the caller,
    /// although every byte before it was flushed.
    #[test]
    pub fn test_finish_reports_a_failure_to_end_the_zstd_frame() {
        let mut writer = BufWriter::new(
            zstd::Encoder::new(FailWhenSet::default(), zstd::DEFAULT_COMPRESSION_LEVEL).unwrap(),
        );
        writer.write_all(DATA.as_bytes()).unwrap();
        writer.flush().unwrap();

        writer.get_mut().get_mut().fail = true;
        let error = finish_zstd(writer).unwrap_err();
        assert_eq!(
            error.kind(),
            io::ErrorKind::StorageFull,
            "the write of the frame end fails"
        );
    }

    /// Writes a few bytes to a plain output on `sink` and finishes it.
    fn finish_plain(sink: FailWhenSet) -> io::Result<()> {
        let mut writer = BufWriter::new(sink);
        writer.write_all(DATA.as_bytes())?;
        finish_buffered(writer).map(drop)
    }

    /// Writes xorshift bytes, which zstd cannot compress, to a `.zst` output
    /// on `sink` and finishes it. The encoder fills its output buffer and
    /// writes to the sink while it takes the buffered bytes.
    fn finish_compressed(sink: FailWhenSet) -> io::Result<()> {
        /// More than the encoder's output buffer.
        const PAYLOAD_SIZE: usize = 1 << 20;
        // xorshift64 with the usual (13, 7, 17) shifts, from an arbitrary
        // nonzero seed.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let payload: Vec<u8> = (0..PAYLOAD_SIZE)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        // A buffer larger than the payload holds every byte until the finish.
        let mut writer = BufWriter::with_capacity(
            2 * PAYLOAD_SIZE,
            zstd::Encoder::new(sink, zstd::DEFAULT_COMPRESSION_LEVEL)?,
        );
        writer.write_all(&payload)?;
        finish_zstd(writer)
    }

    /// After the write of its buffer fails, an output drops the buffer instead
    /// of writing it again.
    #[rstest]
    #[case::plain(finish_plain)]
    #[case::compressed(finish_compressed)]
    pub fn test_a_failed_finish_does_not_write_the_buffer_again(
        #[case] finish: fn(FailWhenSet) -> io::Result<()>,
    ) {
        let failed_writes = Rc::new(Cell::new(0));
        let error = finish(FailWhenSet {
            fail: true,
            failed_writes: Rc::clone(&failed_writes),
        })
        .expect_err("should fail: the sink is full");
        assert_eq!(
            error.kind(),
            io::ErrorKind::StorageFull,
            "the sink's error reaches the caller"
        );
        assert_eq!(failed_writes.get(), 1, "the output writes to the sink once");
    }

    /// A writer dropped during a panic discards its buffered bytes.
    #[test]
    pub fn test_a_writer_dropped_during_a_panic_discards_its_buffer() {
        let out_dir = TempDir::new().unwrap();
        let out_path = out_dir.path().join("panic.jsonl");
        let run = panic::catch_unwind(|| {
            let mut writer = file_writer(&out_path).unwrap();
            writer.write_all(DATA.as_bytes()).unwrap();
            panic!("the run fails while the writer is open");
        });
        assert!(run.is_err(), "the run panics");

        assert_eq!(
            fs::read_to_string(&out_path).unwrap(),
            "",
            "the buffered bytes are not written"
        );
    }

    /// A writer dropped during a panic does not end its zstd frame, so a
    /// reader rejects the output that the panic cut short.
    #[rstest]
    #[case::writing_thread(ZstdThreads::Writer)]
    #[case::worker_threads(ZstdThreads::Pool)]
    pub fn test_a_writer_dropped_during_a_panic_does_not_end_its_frame(
        #[case] threads: ZstdThreads,
    ) {
        let out_dir = TempDir::new().unwrap();
        let out_path = out_dir.path().join("panic.jsonl.zst");
        let run = in_rayon_pool(4, || {
            panic::catch_unwind(|| {
                let mut writer = file_writer_with_zstd_threads(&out_path, threads).unwrap();
                writer.write_all(DATA.as_bytes()).unwrap();
                // Writes the data so far, so the file holds part of a frame.
                writer.flush().unwrap();
                panic!("the run fails while the writer is open");
            })
        });
        assert!(run.is_err(), "the run panics");

        let mut decompressed = String::new();
        let error = file_reader(&out_path)
            .unwrap()
            .read_to_string(&mut decompressed)
            .expect_err("should fail: the frame has no end");
        assert_eq!(
            error.kind(),
            io::ErrorKind::UnexpectedEof,
            "the decoder reaches the end of the file inside the frame: {error}"
        );
    }

    /// A writer created while a panic unwinds, as in the drop of a guard,
    /// finishes when it is dropped.
    #[test]
    pub fn test_a_writer_created_during_a_panic_finishes() {
        struct WriteOnDrop<'a>(&'a Path);

        impl Drop for WriteOnDrop<'_> {
            fn drop(&mut self) {
                let mut writer = file_writer(self.0).unwrap();
                writer.write_all(DATA.as_bytes()).unwrap();
            }
        }

        let out_dir = TempDir::new().unwrap();
        let out_path = out_dir.path().join("report.jsonl.zst");
        let run = panic::catch_unwind(|| {
            let _report = WriteOnDrop(&out_path);
            panic!("the run fails");
        });
        assert!(run.is_err(), "the run panics");

        let mut decompressed = String::new();
        file_reader(&out_path)
            .unwrap()
            .read_to_string(&mut decompressed)
            .unwrap();
        assert_eq!(decompressed, DATA, "the report is one complete zstd frame");
    }

    /// A writer of standard output finishes without an error. The test cannot
    /// see standard output, so it does not show the flush.
    #[test]
    pub fn test_a_standard_output_writer_finishes() {
        FileWriter::stdout()
            .finish()
            .expect("should finish: standard output accepts a flush");
    }

    /// `finish` reports a write failure of a plain or a compressed output.
    #[cfg(target_os = "linux")]
    #[rstest]
    #[case::plain("full.jsonl")]
    #[case::compressed("full.jsonl.zst")]
    pub fn test_finish_reports_a_write_failure(#[case] name: &str) {
        let out_dir = TempDir::new().unwrap();
        let out_path = out_dir.path().join(name);
        symlink("/dev/full", &out_path).unwrap();
        let mut writer = file_writer(&out_path).unwrap();
        writer.write_all(DATA.as_bytes()).unwrap();

        let error = writer.finish().unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("finishing `{}`", out_path.display()),
            "the error names the output"
        );
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::StorageFull),
            "{name}: {error:#}"
        );
    }
}
