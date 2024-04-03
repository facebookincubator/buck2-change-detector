/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

use crate::buck::cells::CellInfo;
use crate::buck::config::is_buck_deployment;
use crate::buck::package_resolver::PackageResolver;
use crate::buck::targets::Targets;
use crate::buck::types::CellName;
use crate::buck::types::CellPath;
use crate::buck::types::Package;
use crate::changes::Changes;
use crate::sapling::status::Status;

/// Do we know for sure the package is present, or is it perhaps missing.
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum PackageStatus {
    Present,
    Unknown,
}

fn is_buckconfig(path: &CellPath) -> bool {
    // Need to match .buckconfig and .bcfg suffix
    // There are also configs from chef etc (e.g. /etc/buckconfig.d/fb_chef.ini)
    // but they won't show up in the change list anyway since they aren't version controlled.
    //
    // If the user passes `@mode/dev` that will change everything.
    // There are also files in `arvr/mode/**/*.inc` that get pulled in to config and places like
    // `tools/buckconfigs/cxx/windows/clang.inc`.
    let ext = path.extension();
    let str = path.as_str();
    ext == Some("bcfg")
        || ext == Some("buckconfig")
        || str.contains("/mode/")
        || str.contains("/buckconfigs/")
}

fn invalidates_graph(path: &CellPath) -> bool {
    is_buckconfig(path) || is_buck_deployment(path)
}

/// Compute the targets we should rerun, or None if we should do everything.
pub fn rerun(
    cells: &CellInfo,
    base: &Targets,
    changes: &Changes,
) -> Option<HashMap<Package, PackageStatus>> {
    // if there are any .buckconfig changes, we should give up
    if changes.cell_paths().any(invalidates_graph) {
        return None;
    }

    let mut res = HashMap::new();
    let all_packages = package_set(base);
    let add_present = |x: HashSet<_>| x.into_iter().map(|x| (x, PackageStatus::Present));

    // targets that are affected due to bzl/build file changes
    let (changed, starlark_changes) = rerun_starlark(cells, base, changes);
    res.extend(add_present(changed));
    // targets that are affected due to PACKAGE file changes
    res.extend(add_present(rerun_package_file(
        changes,
        &starlark_changes,
        &all_packages,
    )));
    // targets that are affected due to source file changes
    res.extend(add_present(rerun_globs(changes, &all_packages)));

    // We extend with this set last, since it may insert PackageStatus::Unknown
    // which need to take precedence over the above.
    // if build file itself appears or disappears
    res.extend(rerun_build_file_existence(cells, changes));
    Some(res)
}

/// Return a set representing the packages
fn package_set(base: &Targets) -> HashSet<&Package> {
    base.imports().filter_map(|x| x.package.as_ref()).collect()
}

/// Figure out which targets should rerun because their import dependencies (or they themselves) changed.
/// Also returns all files which might changed due to Starlark changes.
fn rerun_starlark<'a>(
    cells: &CellInfo,
    base: &'a Targets,
    changes: &'a Changes,
) -> (HashSet<Package>, HashSet<&'a CellPath>) {
    // The key is imported by the files in the value, and maybe corresponds to the Package itself
    let mut rdeps: HashMap<&CellPath, (Option<&Package>, Vec<&CellPath>)> = HashMap::new();
    for i in base.imports() {
        if i.package.is_some() {
            rdeps.entry(&i.file).or_default().0 = i.package.as_ref();
        }
        for import in i.imports.iter() {
            rdeps.entry(import).or_default().1.push(&i.file);
        }
    }

    // Note that you can technically import a non .bzl file, and this handles that (but please don't!)
    // if you change or delete an import, then we should rerun everything that transitively pulls it in
    let dirty_imports = changes.status_cell_paths().filter_map(|x| match x {
        Status::Removed(x) | Status::Modified(x) => Some(x),
        Status::Added(_) => None, // can't possible impact
    });

    // Those we still need to process
    let mut todo: Vec<&CellPath> = dirty_imports.collect();
    // Those which are either in todo, or have been processed
    let mut done: HashSet<&CellPath> = todo.iter().copied().collect();
    let mut res = HashSet::new();

    while let Some(x) = todo.pop() {
        if let Some((package, ds)) = rdeps.get(x) {
            if let Some(package) = package {
                res.insert((*package).clone());
            }
            for d in ds {
                if done.insert(d) {
                    todo.push(d)
                }
            }
        }
    }

    // Also add modified BUCK/TARGETS files
    for change in changes.status_cell_paths() {
        match change {
            Status::Modified(x) if x.is_target_file(cells) => {
                res.insert(Package::new(x.parent().as_str()));
            }
            _ => {}
        }
    }

    (res, done)
}

// `PACKAGE` files are implicitly consulted by all `BUCK` files underneath them.
fn rerun_package_file(
    changes: &Changes,
    starlark_changes: &HashSet<&CellPath>,
    all_packages: &HashSet<&Package>,
) -> HashSet<Package> {
    // We need to go through both `changes` and `starlark_changes`.
    // New files won't yet be on `starlark_changes`, but all modified ones should be.

    // Those package positions that have changed
    let mut changed_package = PackageResolver::new();
    for file in changes.cell_paths().chain(starlark_changes.iter().copied()) {
        if file.is_package_file() {
            changed_package.insert(&file.parent().as_package(), ());
        }
    }

    let mut res = HashSet::new();
    if changed_package.is_empty() {
        return res;
    }

    for p in all_packages {
        if !changed_package.get(p).is_empty() {
            res.insert((*p).clone());
        }
    }
    res
}

// Figure out what packages are affected if a change includes deletion or addition to build files
fn rerun_build_file_existence(
    cells: &CellInfo,
    changes: &Changes,
) -> HashMap<Package, PackageStatus> {
    let mut result = HashMap::new();
    for file in changes.status_cell_paths() {
        // if a build file is changed, put the pattern into query, since buck2 targets only accept either a target or a directory
        let (path, status) = match file {
            Status::Added(x) => (x, PackageStatus::Present),
            Status::Removed(x) => (x, PackageStatus::Unknown),
            Status::Modified(_) => continue,
        };

        if path.is_target_file(cells) {
            let package = Package::new(path.parent().as_str());
            // If we have both Unknown and Present (e.g. BUCK deleted and BUCK.v2 created)
            // we should prefer Present.
            if status == PackageStatus::Unknown {
                result.entry(package).or_insert(status);
            } else {
                result.insert(package, status);
            }
        }
    }
    result
}

/// Figure out which targets should rerun because the list of source files (as visible by glob) changed
fn rerun_globs(changes: &Changes, all_packages: &HashSet<&Package>) -> HashSet<Package> {
    // find parent of the source file and see if it can find a build file
    // recusively look up package that contains this file
    let mut res = HashSet::new();
    for file in changes.status_cell_paths() {
        match file {
            Status::Added(x) | Status::Removed(x) => {
                let cell_relative_path = x.path();
                let path = Path::new(cell_relative_path.as_str()).parent();
                let cell = x.cell();
                let package = find_closest_enclosing_package(path, all_packages, &cell);
                if let Some(p) = package {
                    res.insert(p);
                }
            }
            Status::Modified(_) => {
                // The list of globs does not change if a file gets modified
            }
        }
    }
    res
}

// given a path, return the closest package that includes this path
fn find_closest_enclosing_package(
    mut path: Option<&Path>,
    all_packages: &HashSet<&Package>,
    cell: &CellName,
) -> Option<Package> {
    while let Some(x) = path {
        let potential_package =
            Package::new(&format!("{}//{}", cell.as_str(), x.to_str().unwrap()));
        if all_packages.contains(&potential_package) {
            return Some(potential_package);
        }
        path = x.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buck::targets::BuckError;
    use crate::buck::targets::BuckImport;
    use crate::buck::targets::BuckTarget;
    use crate::buck::targets::TargetsEntry;
    use crate::buck::types::TargetHash;
    use crate::buck::types::TargetLabel;

    #[test]
    fn test_is_buckconfig() {
        assert!(!is_buckconfig(&CellPath::new("fbcode//buck2/TARGETS")));
        assert!(!is_buckconfig(&CellPath::new("fbcode//buck2/src/file.rs")));
        assert!(is_buckconfig(&CellPath::new(
            "fbsource//tools/buckconfigs/cxx/windows/clang.inc"
        )));
        assert!(is_buckconfig(&CellPath::new(
            "fbsource//arvr/mode/dv/dev.buckconfig"
        )));
        assert!(is_buckconfig(&CellPath::new(
            "fbsource//tools/buckconfigs/fbsource-specific.bcfg"
        )));
        assert!(is_buckconfig(&CellPath::new("fbsource//.buckconfig")));
    }

    #[test]
    fn test_rerun_globs() {
        let target_entries = vec![
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("fbcode//pkg/TARGETS"),
                imports: Box::new([
                    CellPath::new("prelude//prelude.bzl"),
                    CellPath::new("fbcode//infra/defs.bzl"),
                ]),
                package: Some(Package::new("fbcode//pkg")),
            }),
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("fbcode//pkg/hello/TARGETS"),
                imports: Box::new([
                    CellPath::new("prelude//hello/prelude.bzl"),
                    CellPath::new("fbcode//hello/infra/defs.bzl"),
                ]),
                package: Some(Package::new("fbcode//pkg/hello")),
            }),
            TargetsEntry::Target(BuckTarget {
                deps: Box::new([
                    TargetLabel::new("toolchains//:python"),
                    TargetLabel::new("fbcode//python:library"),
                ]),
                inputs: Box::new([CellPath::new("fbcode//me/file.bzl")]),
                hash: TargetHash::new("43ce1a7a56f10225413a2991febb853a"),
                ..BuckTarget::testing("test", "", "prelude//rules.bzl:python_library")
            }),
            TargetsEntry::Error(BuckError {
                package: Package::new("fbcode//broken"),
                error: "broken".to_owned(),
            }),
        ];
        let base = Targets::new(target_entries);
        let changes = Changes::testing(&[
            Status::Added(CellPath::new("fbcode//helloworld.cpp")),
            Status::Added(CellPath::new("fbcode//pkg/hello.rs")), // modify fbcode//pkg
            Status::Removed(CellPath::new("fbcode//pkg/world/hello.rs")), // modify fbcode//pkg
            Status::Added(CellPath::new("fbcode//pkg/hello/another.rs")), // modify fbcode//pkg/hello
        ]);
        let changed_package = rerun_globs(&changes, &package_set(&base));
        assert!(changed_package.contains(&Package::new("fbcode//pkg")));
        assert!(changed_package.contains(&Package::new("fbcode//pkg/hello")));
        assert_eq!(changed_package.len(), 2);
    }

    #[test]
    fn test_build_file_changes() {
        let target_entries = vec![
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("fbcode//pkg/TARGETS"),
                imports: Box::new([
                    CellPath::new("prelude//prelude.bzl"),
                    CellPath::new("fbcode//infra/defs.bzl"),
                ]),
                package: Some(Package::new("fbcode//pkg")),
            }),
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("fbcode//pkg/hello/TARGETS"),
                imports: Box::new([
                    CellPath::new("prelude//hello/prelude.bzl"),
                    CellPath::new("fbcode//hello/infra/defs.bzl"),
                ]),
                package: Some(Package::new("fbcode//pkg/hello")),
            }),
            TargetsEntry::Target(BuckTarget {
                deps: Box::new([
                    TargetLabel::new("toolchains//:python"),
                    TargetLabel::new("fbcode//python:library"),
                ]),
                inputs: Box::new([CellPath::new("fbcode//me/file.bzl")]),
                hash: TargetHash::new("43ce1a7a56f10225413a2991febb853a"),
                ..BuckTarget::testing("test", "", "prelude//rules.bzl:python_library")
            }),
            TargetsEntry::Error(BuckError {
                package: Package::new("fbcode//broken"),
                error: "broken".to_owned(),
            }),
        ];
        let base = Targets::new(target_entries);
        let cells = CellInfo::empty();
        let changes =
            Changes::testing(&[Status::Modified(CellPath::new("fbcode//broken/TARGETS"))]);
        let (changed, _) = rerun_starlark(&cells, &base, &changes);
        assert_eq!(changed.len(), 1);
        assert!(changed.contains(&Package::new("fbcode//broken")));
    }

    #[test]
    fn test_rerun_build_file_existence() {
        let changes = Changes::testing(&[
            Status::Added(CellPath::new("foo//b/c/d/BUCK")),
            Status::Removed(CellPath::new("foo//a/b/BUCK.v2")),
            Status::Added(CellPath::new("fbcode//pkg/hello/TARGETS")),
        ]);
        let cells = CellInfo::empty();
        let changed_package = rerun_build_file_existence(&cells, &changes);
        assert_eq!(changed_package.len(), 3);
        assert_eq!(
            changed_package.get(&Package::new("foo//b/c/d")).unwrap(),
            &PackageStatus::Present
        );
        assert_eq!(
            changed_package.get(&Package::new("foo//a/b")).unwrap(),
            &PackageStatus::Unknown
        );
    }

    #[test]
    fn test_more_than_one_build_file() {
        // if a package has more than one build file and only one of them is removed
        // the state of this package is modified
        let changes = Changes::testing(&[Status::Removed(CellPath::new("foo//a/b/c/BUCK.v2"))]);
        let cells = CellInfo::empty();
        let changed_package = rerun_build_file_existence(&cells, &changes);
        assert_eq!(changed_package.len(), 1);
        assert_eq!(
            changed_package.get(&Package::new("foo//a/b/c")).unwrap(),
            &PackageStatus::Unknown
        );
    }

    #[test]
    fn test_more_than_one_build_file_both_removed() {
        // if a package has more than one build file and only one of them is removed
        // the state of this package is removed
        let changes = Changes::testing(&[
            Status::Removed(CellPath::new("foo//a/b/c/BUCK.v2")),
            Status::Removed(CellPath::new("foo//a/b/c/BUCK")),
        ]);
        let cells = CellInfo::empty();
        let changed_package = rerun_build_file_existence(&cells, &changes);
        assert_eq!(changed_package.len(), 1);
        assert_eq!(
            changed_package.get(&Package::new("foo//a/b/c")).unwrap(),
            &PackageStatus::Unknown
        );
    }

    #[test]
    fn test_rerun_package_file() {
        let packages = [
            "foo//bar/baz",
            "foo//bar",
            "foo//bar/inner/more",
            "fbcode//extra/test",
        ];
        let packages: Vec<Package> = packages.iter().map(|x| Package::new(x)).collect();
        let all_packages = packages.iter().collect();

        assert_eq!(
            rerun_package_file(&Changes::default(), &HashSet::new(), &all_packages).len(),
            0
        );
        assert_eq!(
            rerun_package_file(
                &Changes::testing(&[Status::Added(CellPath::new("foo//bar/PACKAGE"))]),
                &HashSet::new(),
                &all_packages
            )
            .len(),
            3
        );
        assert_eq!(
            rerun_package_file(
                &Changes::testing(&[Status::Added(CellPath::new("foo//bar/bar/qux/PACKAGE"))]),
                &HashSet::new(),
                &all_packages
            )
            .len(),
            0
        );
        assert_eq!(
            rerun_package_file(
                &Changes::testing(&[Status::Added(CellPath::new("foo//bar/inner/PACKAGE"))]),
                &HashSet::new(),
                &all_packages
            )
            .len(),
            1
        );
        assert_eq!(
            rerun_package_file(
                &Changes::testing(&[Status::Added(CellPath::new("fbcode//PACKAGE"))]),
                &HashSet::new(),
                &all_packages
            )
            .len(),
            1
        );
    }

    #[test]
    fn test_rerun_package_file_import() {
        let targets = Targets::new(vec![
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("foo//bar/BUCK"),
                imports: Box::new([]),
                package: Some(Package::new("foo//bar")),
            }),
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("foo//PACKAGE"),
                imports: Box::new([CellPath::new("foo//utils.bzl")]),
                package: None,
            }),
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("foo//utils.bzl"),
                imports: Box::new([]),
                package: None,
            }),
        ]);
        let cells = CellInfo::empty();
        let changes = Changes::testing(&[Status::Modified(CellPath::new("foo//utils.bzl"))]);

        assert_eq!(
            rerun_package_file(
                &changes,
                &rerun_starlark(&cells, &targets, &changes).1,
                &package_set(&targets)
            )
            .len(),
            1
        );
    }

    #[test]
    fn test_rerun_e2e() {
        let target_entries = vec![
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("foo//a/b/c/BUCK.v2"),
                imports: Box::new([]),
                package: Some(Package::new("foo//a/b/c")),
            }),
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("foo//a/b/c/BUCK"),
                imports: Box::new([]),
                package: Some(Package::new("foo//a/b/c")),
            }),
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("bar//b/BUCK"),
                imports: Box::new([]),
                package: Some(Package::new("bar//b")),
            }),
        ];
        let base = Targets::new(target_entries);
        let cells = CellInfo::empty();
        let changes = Changes::testing(&[
            Status::Removed(CellPath::new("foo//a/b/c/BUCK.v2")),
            Status::Removed(CellPath::new("foo//a/b/c/BUCK")),
            Status::Removed(CellPath::new("foo//a/b/c/hello.cpp")),
            Status::Added(CellPath::new("bar//b/c/d.cpp")),
            Status::Added(CellPath::new("bar//a/BUCK")),
        ]);
        let rerun_result = rerun(&cells, &base, &changes).unwrap();
        assert_eq!(rerun_result.len(), 3);
        assert_eq!(
            rerun_result.get(&Package::new("foo//a/b/c")).unwrap(),
            &PackageStatus::Unknown
        );
        assert_eq!(
            rerun_result.get(&Package::new("bar//b")).unwrap(),
            &PackageStatus::Present
        );
        assert_eq!(
            rerun_result.get(&Package::new("bar//a")).unwrap(),
            &PackageStatus::Present
        );
    }
}
