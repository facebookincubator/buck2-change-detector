/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::Context as _;

use crate::buck::types::CellName;
use crate::buck::types::CellPath;
use crate::buck::types::CellRelativePath;
use crate::buck::types::ProjectRelativePath;

#[derive(Debug)]
struct CellData {
    path: ProjectRelativePath,
    build_files: Vec<String>,
}

#[derive(Debug)]
pub struct CellInfo {
    cells: HashMap<CellName, CellData>,
    /// Sorted by path length, so the longest is first
    paths: Vec<(CellName, ProjectRelativePath)>,
}

impl CellInfo {
    /// A default `CellInfo` for use in tests.
    pub fn testing() -> Self {
        // We'd really like this to be `#[cfg(any(test, doctest))]`, but that doesn't work
        // because of https://github.com/rust-lang/rust/issues/67295.
        Self {
            cells: Default::default(),
            paths: Default::default(),
        }
    }

    pub fn new(file: &Path) -> anyhow::Result<Self> {
        let data = fs::read_to_string(file)
            .with_context(|| format!("When reading `{}`", file.display()))?;
        Self::parse(&data)
    }

    fn parse_cells_data(data: &str) -> anyhow::Result<HashMap<CellName, CellData>> {
        let json: HashMap<String, String> = serde_json::from_str(data)?;

        // We need to find the shortest path, as that will be the prefix and we want project relative paths
        let prefix = json
            .values()
            .min_by_key(|x| x.len())
            .ok_or_else(|| anyhow::anyhow!("Empty JSON object for the cells"))?
            .to_owned();
        let mut cells = HashMap::with_capacity(json.len());
        for (k, v) in json.into_iter() {
            match v.strip_prefix(&prefix) {
                None => {
                    return Err(anyhow::anyhow!(
                        "Expected key `{k}` to start with `{prefix}`, but got `{v}`"
                    ));
                }
                Some(rest) => {
                    cells.insert(
                        CellName::new(&k),
                        CellData {
                            path: ProjectRelativePath::new(rest.trim_start_matches('/')),
                            build_files: Self::default_build_files(&k)
                                .iter()
                                .map(|x| (*x).to_owned())
                                .collect(),
                        },
                    );
                }
            }
        }
        Ok(cells)
    }

    fn create_paths(cells: &HashMap<CellName, CellData>) -> Vec<(CellName, ProjectRelativePath)> {
        let mut paths = cells
            .iter()
            .map(|(k, v)| ((*k).clone(), v.path.clone()))
            .collect::<Vec<_>>();
        paths.sort_by_key(|x| -(x.1.as_str().len() as isize));
        paths
    }

    pub fn parse(data: &str) -> anyhow::Result<Self> {
        let cells = Self::parse_cells_data(data)?;
        let paths = Self::create_paths(&cells);
        Ok(Self { cells, paths })
    }

    pub fn load_config_data(&mut self, file: &Path) -> anyhow::Result<()> {
        let data = fs::read_to_string(file)
            .with_context(|| format!("When reading `{}`", file.display()))?;
        self.parse_config_data(&data)
    }

    pub fn parse_config_data(&mut self, data: &str) -> anyhow::Result<()> {
        let json: HashMap<String, String> = serde_json::from_str(data)?;
        // name_v2 needs to take precedence, so evaluate it second
        for v2 in [false, true] {
            let want_key = if v2 {
                "buildfile.name_v2"
            } else {
                "buildfile.name"
            };
            for (k, v) in json.iter() {
                // Expecting `cell//buildfile.name` = `BUCK,TARGETS`
                if let Some((cell, key)) = k.split_once("//") {
                    if key == want_key {
                        let mut names = v
                            .split(',')
                            .map(|x| x.trim().to_owned())
                            .collect::<Vec<_>>();
                        if !v2 {
                            // For name, we infer the .v2 suffix automatically
                            names = names
                                .into_iter()
                                .flat_map(|x| [format!("{x}.v2"), x])
                                .collect();
                        }
                        if let Some(data) = self.cells.get_mut(&CellName::new(cell)) {
                            data.build_files = names;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn resolve(&self, path: &CellPath) -> anyhow::Result<ProjectRelativePath> {
        match self.cells.get(&path.cell()) {
            Some(data) => Ok(data.path.join(path.path().as_str())),
            None => Err(anyhow::anyhow!("Unknown cell, `{path}`")),
        }
    }

    pub fn unresolve(&self, path: &ProjectRelativePath) -> anyhow::Result<CellPath> {
        // because we know self.paths has the longest match first, we just find the first match
        for (cell, prefix) in &self.paths {
            if let Some(x) = path.as_str().strip_prefix(prefix.as_str()) {
                let x = x.strip_prefix('/').unwrap_or(x);
                return Ok(cell.join(&CellRelativePath::new(x)));
            }
        }
        Err(anyhow::anyhow!(
            "Path has no cell which is a prefix `{path}`"
        ))
    }

    /// The default build files that we hardcode for now.
    fn default_build_files(cell: &str) -> &'static [String] {
        // TODO: We eventually want to remove the hardcoding
        if cell == "fbcode" || cell == "prelude" || cell == "toolchains" {
            static RESULT: LazyLock<Vec<String>> =
                LazyLock::new(|| vec!["TARGETS.v2".to_owned(), "TARGETS".to_owned()]);
            &RESULT
        } else {
            static RESULT: LazyLock<Vec<String>> =
                LazyLock::new(|| vec!["BUCK.v2".to_owned(), "BUCK".to_owned()]);
            &RESULT
        }
    }

    pub fn build_files(&self, cell: &CellName) -> &[String] {
        match self.cells.get(cell) {
            Some(data) => &data.build_files,
            None => Self::default_build_files(cell.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cell() {
        let value = serde_json::json!(
            {
                "inner1": "/Users/ndmitchell/repo/inner1",
                "inner2": "/Users/ndmitchell/repo/inner1/inside/inner2",
                "root": "/Users/ndmitchell/repo",
                "prelude": "/Users/ndmitchell/repo/prelude"
              }
        );
        let cells = CellInfo::parse(&serde_json::to_string(&value).unwrap()).unwrap();

        fn testcase(cells: &CellInfo, cell_path: &str, project_relative_path: &str) {
            let cell_path = CellPath::new(cell_path);
            let project_relative_path = ProjectRelativePath::new(project_relative_path);
            assert_eq!(cells.resolve(&cell_path).unwrap(), project_relative_path);
            assert_eq!(cells.unresolve(&project_relative_path).unwrap(), cell_path);
        }

        testcase(&cells, "inner1//magic/file.txt", "inner1/magic/file.txt");
        testcase(
            &cells,
            "inner2//magic/file.txt",
            "inner1/inside/inner2/magic/file.txt",
        );
        testcase(&cells, "root//file.txt", "file.txt");

        assert!(cells.resolve(&CellPath::new("missing//foo.txt")).is_err());
    }

    #[test]
    fn test_cell_config() {
        let value = serde_json::json!(
            {
                "root": "/Users/ndmitchell/repo",
                "cell1": "/Users/ndmitchell/repo/cell1",
                "cell2": "/Users/ndmitchell/repo/cell2",
              }
        );
        let mut cells = CellInfo::parse(&serde_json::to_string(&value).unwrap()).unwrap();
        let value = serde_json::json!(
            {
                "cell1//buildfile.name":"BUCK",
                "cell1//buildfile.name_v2":"TARGETS",
                "cell2//buildfile.name":"A1,A2",
            }
        );
        cells
            .parse_config_data(&serde_json::to_string(&value).unwrap())
            .unwrap();
        assert_eq!(cells.build_files(&CellName::new("cell1")), &["TARGETS"]);
        assert_eq!(
            cells.build_files(&CellName::new("cell2")),
            &["A1.v2", "A1", "A2.v2", "A2"]
        );
        assert_eq!(
            cells.build_files(&CellName::new("cell3")),
            &["BUCK.v2", "BUCK"]
        );
    }
}
