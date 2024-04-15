/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Utilities for working with JSON and JSON-lines files.

use std::fs::File;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Context as _;
use serde::Deserialize;
use serde::Serialize;

// Function definition mostly to get the error types to line up
fn parse_line<T: for<'a> Deserialize<'a>>(x: Result<String, io::Error>) -> anyhow::Result<T> {
    let x = x?;
    serde_json::from_str(&x).with_context(|| format!("When parsing: {x}"))
}

fn is_zstd(filename: &Path) -> bool {
    match filename.extension() {
        Some(x) => x == "zst",
        None => false,
    }
}

fn open_file(filename: &Path) -> anyhow::Result<impl BufRead + Send> {
    let file = File::open(filename)?;
    if is_zstd(filename) {
        Ok(Box::new(BufReader::new(zstd::Decoder::new(file)?)) as Box<dyn BufRead + Send>)
    } else {
        Ok(Box::new(BufReader::new(file)))
    }
}

/// Read a file that consists of many JSON blobs, one per line.
/// The order of the entries does not matter.
pub fn read_file_lines_unordered<T: for<'a> Deserialize<'a> + Send>(
    filename: &Path,
) -> anyhow::Result<Vec<T>> {
    fn f<T: for<'a> Deserialize<'a> + Send>(filename: &Path) -> anyhow::Result<Vec<T>> {
        let result = Mutex::new(Vec::new());
        let error = Mutex::new(None);
        let file = open_file(filename)?;

        rayon::scope(|s| {
            for x in file.lines() {
                s.spawn(|_| match parse_line(x) {
                    Err(e) => {
                        error.lock().unwrap().get_or_insert(e);
                    }
                    Ok(v) => result.lock().unwrap().push(v),
                });
            }
        });

        if let Some(err) = error.into_inner().unwrap() {
            return Err(err);
        }
        Ok(result.into_inner().unwrap())
    }
    f(filename).with_context(|| format!("When reading JSON-lines file `{}`", filename.display()))
}

/// Read a file that consists of many JSON blobs, one per line.
pub fn read_file_lines<T: for<'a> Deserialize<'a>>(filename: &Path) -> anyhow::Result<Vec<T>> {
    fn f<T: for<'a> Deserialize<'a>>(filename: &Path) -> anyhow::Result<Vec<T>> {
        let file = open_file(filename)?;
        let mut res = Vec::new();
        for line in file.lines() {
            res.push(parse_line(line)?)
        }
        Ok(res)
    }

    f(filename).with_context(|| format!("When reading file `{}`", filename.display()))
}

/// Write out information as a list of JSON lines.
pub fn write_json_lines<W: Write, T: Serialize>(
    mut out: W,
    xs: impl IntoIterator<Item = T>,
) -> anyhow::Result<()> {
    for x in xs.into_iter() {
        serde_json::to_writer(&mut out, &x)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
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
        first.serialize(&mut serde_json::Serializer::new(&mut out))?;
        for x in it {
            out.write_all(b",\n  ")?;
            x.serialize(&mut serde_json::Serializer::new(&mut out))?;
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
    use crate::json::read_file_lines_unordered;
    use crate::json::write_json_lines;
    use crate::json::write_json_per_line;

    #[test]
    fn test_json_lines() {
        let mut file = NamedTempFile::new().unwrap();
        let data: Vec<i32> = (0..100).collect();
        write_json_lines(file.as_file_mut(), &data).unwrap();
        assert_eq!(read_file_lines::<i32>(file.path()).unwrap(), data);
        let mut unordered = read_file_lines_unordered::<i32>(file.path()).unwrap();
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

        assert!(read_file_lines_unordered::<i32>(file.path()).is_err());
        assert!(read_file_lines::<i32>(file.path()).is_err());
    }
}
