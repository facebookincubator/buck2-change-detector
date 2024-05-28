/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashSet;

use td_util::prelude::*;

use crate::buck::cells::CellInfo;
use crate::buck::types::CellPath;
use crate::buck::types::Package;
use crate::buck::types::ProjectRelativePath;
use crate::sapling::status::Status;

#[derive(Default, Debug)]
pub struct Changes {
    paths: Vec<Status<(CellPath, ProjectRelativePath)>>,
    cell_paths_set: HashSet<CellPath>,
}

impl Changes {
    pub fn new(
        cells: &CellInfo,
        changes: Vec<Status<ProjectRelativePath>>,
    ) -> anyhow::Result<Self> {
        let paths =
            changes.into_try_map(|x| x.into_try_map(|x| anyhow::Ok((cells.unresolve(&x)?, x))))?;
        Ok(Self::from_paths(paths))
    }

    fn from_paths(paths: Vec<Status<(CellPath, ProjectRelativePath)>>) -> Self {
        let cell_paths_set = paths.iter().map(|x| x.get().0.clone()).collect();
        Self {
            paths,
            cell_paths_set,
        }
    }

    #[cfg(test)]
    pub fn testing(changes: &[Status<CellPath>]) -> Self {
        fn mk_project_path(path: &CellPath) -> ProjectRelativePath {
            ProjectRelativePath::new(path.path().as_str())
        }

        let paths = changes.map(|x| x.map(|x| (x.clone(), mk_project_path(x))));
        Self::from_paths(paths)
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn status_cell_paths(&self) -> impl Iterator<Item = Status<&CellPath>> {
        self.paths.iter().map(|x| x.map(|x| &x.0))
    }

    pub fn cell_paths(&self) -> impl Iterator<Item = &CellPath> {
        self.paths.iter().map(|x| &x.get().0)
    }

    pub fn project_paths(&self) -> impl Iterator<Item = &ProjectRelativePath> {
        self.paths.iter().map(|x| &x.get().1)
    }

    pub fn contains_cell_path(&self, path: &CellPath) -> bool {
        self.cell_paths_set.contains(path)
    }

    pub fn contains_package(&self, package: &Package) -> bool {
        self.contains_cell_path(&package.as_cell_path())
    }

    pub fn filter_by_extension(&self, f: impl Fn(Option<&str>) -> bool) -> Changes {
        let paths = self
            .paths
            .iter()
            .filter(|x| f(x.get().0.extension()))
            .cloned()
            .collect();
        Self::from_paths(paths)
    }
}
