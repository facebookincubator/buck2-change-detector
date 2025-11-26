/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::HashMap;
use std::collections::HashSet;

use serde::Serialize;
use td_util::no_hash::BuildNoHash;
use td_util_buck::package_resolver::PackageResolver;
use td_util_buck::targets::BuckTarget;
use td_util_buck::targets::Targets;
use td_util_buck::types::Package;
use td_util_buck::types::TargetLabel;
use td_util_buck::types::TargetPattern;
use thiserror::Error;
use tracing::error;
use tracing::warn;

use crate::changes::Changes;
use crate::diff::ImpactTraceData;

#[derive(Debug, Error, Serialize)]
pub enum ValidationError {
    #[error("Package `{package}` failed with error produced by Buck2:\n{error}")]
    PackageFailed { package: Package, error: String },
    #[error(
        "Package `{package}` failed with error produced by Buck2 (it also failed in the base revision, so perhaps rebase):\n{error}"
    )]
    PreexistingPackageFailed { package: Package, error: String },
    #[error("Target `{deleted}` was deleted but is referenced by `{referenced_by}`")]
    TargetDeleted {
        deleted: TargetLabel,
        referenced_by: TargetLabel,
    },
    #[error(
        "Target `{referenced_by}` has a dangling dependency. `{missing}` was not in the graph."
    )]
    BrokenEdge {
        missing: TargetLabel,
        referenced_by: TargetLabel,
    },
}

fn in_universe(universe: &[TargetPattern], dep: &TargetLabel) -> bool {
    universe.is_empty() || universe.iter().any(|p| p.matches(dep))
}

/// We want to track existing issues in the graph so we can keep it as
/// low as possible. But limit dangling edges within the universe, since
/// the edges outside the universe are impossible to validate by construction.
pub fn dump_all_errors(graph: &Targets, universe: &[TargetPattern]) -> Vec<ValidationError> {
    // Collect all the parse errors first.
    let mut all_errors: Vec<ValidationError> = graph
        .errors()
        .map(|err| ValidationError::PackageFailed {
            package: err.package.clone(),
            error: err.error.clone(),
        })
        .collect();

    let existing_targets = graph.targets_by_label();

    for x in graph.targets() {
        for dep in x.deps.iter() {
            if !existing_targets.contains_key(dep) && in_universe(universe, dep) {
                all_errors.push(ValidationError::BrokenEdge {
                    missing: dep.clone(),
                    referenced_by: x.label(),
                });
            }
        }
    }

    all_errors
}

/// We want to be resiliant to pre-existing breakages, but we complain if:
///
/// 1. You have new errors, because you introduced them.
/// 2. The errors are in a package that you changed, because that will probably stop
///    accurate tests being run for your code.
pub fn check_errors(base: &Targets, diff: &Targets, changes: &Changes) -> Vec<ValidationError> {
    let mut diff_errors = HashMap::new();
    let mut errors_tree = PackageResolver::new();
    for err in diff.errors() {
        diff_errors.insert(&err.package, &err.error);
        errors_tree.insert(&err.package, (&err.package, &err.error));
    }

    for err in base.errors() {
        if let Some(diff_err) = diff_errors.remove(&err.package) {
            if diff_err != &err.error {
                // We could say that a change of error means that it is a fresh break.
                // But error messages might be non-deterministic in some circumstances, so let them through.
                warn!(
                    "Error for package `{}` has changed, was:\n{}\nNow:\n{}",
                    err.package, err.error, diff_err
                );
            }
        }
    }

    let mut res: Vec<_> = diff_errors
        .iter()
        .map(|(package, error)| ValidationError::PackageFailed {
            package: (*package).clone(),
            error: (*error).clone(),
        })
        .collect();

    // If there are errors which you caused, and also preexisting errors that happen to impact you
    // then the first are ones you can directly fix, the second are more of a pain and hopefully will
    // disappear on a rebase anyway. So just report the former.
    if !res.is_empty() {
        return res;
    }

    let mut bad_packages = HashSet::with_hasher(BuildNoHash::default());
    for path in changes.cell_paths() {
        if let Some((package, err)) = errors_tree.get(&path.as_package()).pop() {
            if bad_packages.insert(package) {
                res.push(ValidationError::PreexistingPackageFailed {
                    package: (*package).clone(),
                    error: (*err).clone(),
                })
            }
        }
    }

    res
}

fn check_deleted_edges<'a, I>(
    edges: I,
    target: &BuckTarget,
    universe: &[TargetPattern],
    deleted: &mut HashSet<TargetLabel, BuildNoHash>,
    errors: &mut Vec<ValidationError>,
) where
    I: Iterator<Item = &'a TargetLabel>,
{
    for edge in edges {
        // remove so that we only report each target at most once
        if in_universe(universe, edge) && deleted.remove(edge) {
            errors.push(ValidationError::TargetDeleted {
                deleted: edge.clone(),
                referenced_by: target.label(),
            });
        }
    }
}

fn check_broken_edges<'a, I>(
    edges: I,
    target: &BuckTarget,
    exists_after: &HashMap<
        td_util_buck::types::TargetLabelKeyRef,
        &BuckTarget,
        impl std::hash::BuildHasher,
    >,
    universe: &[TargetPattern],
    base_edges: &[TargetLabel],
    errors: &mut Vec<ValidationError>,
) where
    I: Iterator<Item = &'a TargetLabel>,
{
    for edge in edges {
        let key = edge.key();
        // Only check newly introduced dangling dependencies that are
        // within our universe.
        if !exists_after.contains_key(&key.to_ref())
            && !base_edges.iter().any(|e| e == edge)
            && in_universe(universe, edge)
        {
            errors.push(ValidationError::BrokenEdge {
                missing: edge.clone(),
                referenced_by: target.label(),
            });
        }
    }
}

/// If you remove a target which is referenced by other people, that is bad.
/// We don't require a complete valid graph, as that's too much to hope for.
/// If you add a dangling dependency, that's also bad.
pub fn check_dangling(
    base: &Targets,
    diff: &Targets,
    immediate_changes: &[(&BuckTarget, ImpactTraceData)],
    universe: &[TargetPattern],
) -> Vec<ValidationError> {
    let exists_after = diff.targets_by_label_key();
    let base_targets_map = base.targets_by_label_key();

    let mut errors = Vec::new();
    // Lets check if dangling edges were introduced.
    for (target, _) in immediate_changes.iter() {
        let base_deps = base_targets_map
            .get(&target.label_key())
            .map(|t| t.deps.as_ref())
            .unwrap_or(&[]);

        // checks for broken edges in the deps
        check_broken_edges(
            target.deps.iter(),
            target,
            &exists_after,
            universe,
            base_deps,
            &mut errors,
        );
    }

    let mut deleted = HashSet::with_hasher(BuildNoHash::default());
    for x in base.targets() {
        if !exists_after.contains_key(&x.label_key()) {
            deleted.insert(x.label());
        }
    }

    // Avoid iterating on the full graph.
    if deleted.is_empty() {
        return errors;
    }

    // now lets see if any of those we deleted show up
    for x in diff.targets() {
        check_deleted_edges(x.deps.iter(), x, universe, &mut deleted, &mut errors);
    }

    errors
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use td_util::prelude::*;
    use td_util_buck::targets::BuckError;
    use td_util_buck::targets::TargetsEntry;
    use td_util_buck::types::CellPath;
    use td_util_buck::types::TargetName;

    use super::*;
    use crate::sapling::status::Status;

    #[test]
    fn test_check_errors_changed() {
        // We need to make sure that if an error appears, we fail
        let err_bar0 = &TargetsEntry::Error(BuckError {
            package: Package::new("foo//bar"),
            error: "Bad 0".to_owned(),
        });
        let err_bar1 = &TargetsEntry::Error(BuckError {
            package: Package::new("foo//bar"),
            error: "Bad 1".to_owned(),
        });
        let err_baz = &TargetsEntry::Error(BuckError {
            package: Package::new("foo//baz"),
            error: "Bad 2".to_owned(),
        });

        fn errs(base: &[&TargetsEntry], diff: &[&TargetsEntry]) -> Vec<ValidationError> {
            check_errors(
                &Targets::new(base.iter().copied().cloned().collect()),
                &Targets::new(diff.iter().copied().cloned().collect()),
                &Changes::default(),
            )
        }

        assert_eq!(errs(&[], &[err_bar0]).len(), 1);
        assert_eq!(errs(&[err_baz], &[err_bar0]).len(), 1);
        assert_eq!(errs(&[], &[err_bar1, err_baz]).len(), 2);
        assert_eq!(errs(&[err_bar0], &[err_bar0]).len(), 0);
        assert_eq!(errs(&[err_bar0], &[]).len(), 0);
        assert_eq!(errs(&[err_bar1, err_baz], &[]).len(), 0);
        // This one is debatable, the error changed between base and diff, but is in the same package.
        // Because error messages might be non-deterministic we should keep it.
        assert_eq!(errs(&[err_bar1], &[err_bar0]).len(), 0);
    }

    #[test]
    fn test_check_errors_impactful() {
        // Any errors in packages above us should cause a failure, since our code is a bit broken
        let error0 = TargetsEntry::Error(BuckError {
            package: Package::new("foo//bar"),
            error: "Bad 0".to_owned(),
        });
        let error1 = TargetsEntry::Error(BuckError {
            package: Package::new("foo//bar/baz"),
            error: "Bad 2".to_owned(),
        });
        let targets = Targets::new(vec![error0, error1]);
        assert_eq!(
            check_errors(
                &targets,
                &targets,
                &Changes::testing(&[Status::Modified(CellPath::new("foo//bar/baz/qux/file.txt"))])
            )
            .len(),
            1
        );
        assert_eq!(
            check_errors(
                &targets,
                &targets,
                &Changes::testing(&[Status::Modified(CellPath::new("foo//bar/file.txt"))])
            )
            .len(),
            1
        );
        assert_eq!(
            check_errors(
                &targets,
                &targets,
                &Changes::testing(&[Status::Modified(CellPath::new("foo//other/file.txt"))])
            )
            .len(),
            0
        );
    }

    fn target_target(name: &str, deps: &[&str]) -> BuckTarget {
        BuckTarget {
            deps: deps
                .iter()
                .map(|x| Package::new("foo//bar").join(&TargetName::new(x)))
                .collect(),
            ..BuckTarget::testing(name, "foo//bar", "prelude//rules.bzl:cxx_library")
        }
    }

    fn target_entry(name: &str, deps: &[&str]) -> TargetsEntry {
        TargetsEntry::Target(target_target(name, deps))
    }

    #[rstest]
    // Delete target and its deps - OK
    #[case::delete_target_and_deps(
        vec![
            target_entry("aaa", &[]),
            target_entry("bbb", &["aaa", "ccc"]),
            target_entry("ccc", &[]),
        ],
        vec![
            target_entry("bbb", &["ccc"]),
            target_entry("ccc", &[]),
        ],
        vec![],
        vec![TargetPattern::new("foo//...")],
        0
    )]
    // Delete target with no deps - OK
    #[case::delete_target_no_deps(
        vec![target_entry("aaa", &[]), target_entry("bbb", &[])],
        vec![target_entry("bbb", &[])],
        vec![],
        vec![TargetPattern::new("foo//...")],
        0
    )]
    // Delete target but leave its deps - BAD
    #[case::delete_target_leave_deps(
        vec![
            target_entry("aaa", &[]),
            target_entry("bbb", &["aaa"]),
        ],
        vec![target_entry("bbb", &["aaa"])],
        vec![],
        vec![TargetPattern::new("foo//...")],
        1
    )]
    // Don't error when deleted dependency is outside universe
    #[case::deleted_dep_outside_universe(
        vec![
            target_entry("aaa", &[]),
            target_entry("bbb", &["aaa"]),
        ],
        vec![target_entry("bbb", &["aaa"])],
        vec![],
        vec![TargetPattern::new("bar//...")],
        0
    )]
    // Dangling edges on dep addition - BAD
    #[case::dangling_on_dep_addition(
        vec![
            target_entry("aaa", &[]),
            target_entry("bbb", &["aaa"])
        ],
        vec![
            target_entry("aaa", &[]),
            target_entry("bbb", &["aaa", "ccc"])
        ],
        vec![target_target("bbb", &["aaa", "ccc"])],
        vec![TargetPattern::new("foo//...")],
        1
    )]
    // Dangling edges on target addition - BAD
    #[case::dangling_on_target_addition(
        vec![
            target_entry("aaa", &[]),
            target_entry("bbb", &["aaa"])
        ],
        vec![
            target_entry("aaa", &[]),
            target_entry("bbb", &["aaa"]),
            target_entry("ccc", &["ddd"])
        ],
        vec![target_target("ccc", &["ddd"])],
        vec![TargetPattern::new("foo//...")],
        1
    )]
    // Don't error on pre-existing dangling edges
    #[case::preexisting_dangling_edges(
        vec![
            target_entry("aaa", &["ccc"]),
            target_entry("bbb", &["aaa"])
        ],
        vec![
            target_entry("aaa", &["ccc"]),
            target_entry("bbb", &["aaa"])
        ],
        vec![target_target("bbb", &["aaa"])],
        vec![TargetPattern::new("foo//...")],
        0
    )]
    // Don't error even if we modify target with dangling edge
    #[case::modify_target_with_dangling_edge(
        vec![
            target_entry("aaa", &["ccc"]),
            target_entry("bbb", &["aaa"])
        ],
        vec![
            target_entry("aaa", &["ccc"]),
            target_entry("bbb", &["aaa"])
        ],
        vec![target_target("aaa", &["ccc"])],
        vec![TargetPattern::new("foo//...")],
        0
    )]
    // No error if we fix the missing edge
    #[case::fix_missing_edge(
        vec![
            target_entry("aaa", &["ccc"]),
            target_entry("bbb", &["aaa"])
        ],
        vec![
            target_entry("aaa", &[]),
            target_entry("bbb", &["aaa"])
        ],
        vec![target_target("aaa", &[])],
        vec![TargetPattern::new("foo//...")],
        0
    )]
    fn test_check_dangling(
        #[case] base_entries: Vec<TargetsEntry>,
        #[case] diff_entries: Vec<TargetsEntry>,
        #[case] modified_targets: Vec<BuckTarget>,
        #[case] universe: Vec<TargetPattern>,
        #[case] expected_error_count: usize,
    ) {
        let immediate_changes: Vec<_> = modified_targets
            .iter()
            .map(|t| (t, ImpactTraceData::testing()))
            .collect();

        assert_eq!(
            check_dangling(
                &Targets::new(base_entries),
                &Targets::new(diff_entries),
                &immediate_changes,
                &universe,
            )
            .len(),
            expected_error_count
        );
    }

    #[test]
    fn test_dump_all_errors() {
        // We need to make sure that if an error appears, we fail
        let error0 = TargetsEntry::Error(BuckError {
            package: Package::new("foo//bar"),
            error: "Bad 0".to_owned(),
        });
        let error1 = TargetsEntry::Error(BuckError {
            package: Package::new("foo//bar"),
            error: "Bad 1".to_owned(),
        });
        let error2 = TargetsEntry::Error(BuckError {
            package: Package::new("foo//baz"),
            error: "Bad 2".to_owned(),
        });
        let good0 = TargetsEntry::Target(BuckTarget::testing("target0", "foo//good", "rule"));
        let good1 = TargetsEntry::Target(BuckTarget {
            deps: Box::new([Package::new("foo//good").join(&TargetName::new("target0"))]),
            ..BuckTarget::testing("target1", "foo//good", "rule")
        });
        let dangling0 = TargetsEntry::Target(BuckTarget {
            deps: Box::new([
                Package::new("foo//good").join(&TargetName::new("target0")),
                Package::new("foo//good").join(&TargetName::new("missing")),
            ]),
            ..BuckTarget::testing("target-with-dangling", "foo//good", "rule")
        });
        let dangling1 = TargetsEntry::Target(BuckTarget {
            deps: Box::new([Package::new("outside//bar").join(&TargetName::new("target0"))]),
            ..BuckTarget::testing("other-with-dangling", "foo//good", "rule")
        });
        let targets = [error0, error1, error2, good0, good1, dangling0, dangling1];

        let errs = |xs: &[usize]| Targets::new(xs.map(|i| targets[*i].clone()));

        let universe = [TargetPattern::new("foo//...")];
        // We report all errors per package.
        assert_eq!(dump_all_errors(&errs(&[0, 1]), &universe).len(), 2);
        assert_eq!(dump_all_errors(&errs(&[0, 1, 2]), &universe).len(), 3);
        assert_eq!(
            dump_all_errors(&errs(&[0, 1, 2, 3, 4, 5]), &universe).len(),
            4
        );
        // We report dangling edges within the universe.
        assert_eq!(dump_all_errors(&errs(&[3, 5]), &universe).len(), 1);
        assert_eq!(dump_all_errors(&errs(&[3, 4]), &universe).len(), 0);
        // Error is outside the universe, so don't report it.
        assert_eq!(dump_all_errors(&errs(&[3, 4, 6]), &universe).len(), 0);
        assert_eq!(dump_all_errors(&errs(&[3, 5, 6]), &universe).len(), 1);
        // Different universe discovers the error.
        assert_eq!(
            dump_all_errors(&errs(&[3, 4, 6]), &[TargetPattern::new("outside//...")]).len(),
            1
        );
    }
}
