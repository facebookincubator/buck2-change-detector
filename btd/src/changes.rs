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
    cell_paths: Vec<Status<CellPath>>,
    cell_paths_set: HashSet<CellPath>,
    project_paths: Vec<Status<ProjectRelativePath>>,
}

impl Changes {
    pub fn new(
        cells: &CellInfo,
        changes: Vec<Status<ProjectRelativePath>>,
    ) -> anyhow::Result<Self> {
        let cell_paths = changes.try_map(|x| x.try_map(|x| cells.unresolve(x)))?;
        let cell_paths_set = cell_paths.iter().map(|x| x.get().clone()).collect();
        Ok(Self {
            cell_paths,
            cell_paths_set,
            project_paths: changes,
        })
    }

    #[cfg(test)]
    pub fn testing(changes: &[Status<CellPath>]) -> Self {
        fn mk_project_path(path: &CellPath) -> ProjectRelativePath {
            ProjectRelativePath::new(path.path().as_str())
        }

        let cell_paths = changes.to_owned();
        let cell_paths_set = cell_paths.iter().map(|x| x.get().clone()).collect();
        let project_paths = changes.map(|x| x.map(mk_project_path));
        Self {
            cell_paths,
            cell_paths_set,
            project_paths,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.cell_paths.is_empty()
    }

    pub fn status_cell_paths(&self) -> impl Iterator<Item = Status<&CellPath>> {
        self.cell_paths.iter().map(|x| x.map(|x| x))
    }

    pub fn cell_paths(&self) -> impl Iterator<Item = &CellPath> {
        self.cell_paths.iter().map(|x| x.get())
    }

    pub fn project_paths(&self) -> impl Iterator<Item = &ProjectRelativePath> {
        self.project_paths.iter().map(|x| x.get())
    }

    pub fn contains_cell_path(&self, path: &CellPath) -> bool {
        self.cell_paths_set.contains(path)
    }

    pub fn contains_package(&self, package: &Package) -> bool {
        self.contains_cell_path(&package.as_cell_path())
    }

    pub fn filter_by_extension(&self, f: impl Fn(Option<&str>) -> bool) -> Changes {
        let cell_paths = self
            .cell_paths
            .iter()
            .filter(|x| f(x.get().extension()))
            .cloned()
            .collect::<Vec<_>>();
        let cell_paths_set = cell_paths.iter().map(|x| x.get().clone()).collect();
        let project_paths = self
            .project_paths
            .iter()
            .filter(|x| f(x.get().extension()))
            .cloned()
            .collect();
        Changes {
            cell_paths,
            cell_paths_set,
            project_paths,
        }
    }
}
