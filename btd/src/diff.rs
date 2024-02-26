/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::HashSet;
use std::mem;

use tracing::warn;

use crate::buck::glob::GlobSpec;
use crate::buck::target_map::TargetMap;
use crate::buck::targets::BuckTarget;
use crate::buck::targets::TargetLabelKey;
use crate::buck::targets::Targets;
use crate::buck::types::CellPath;
use crate::buck::types::Glob;
use crate::buck::types::Package;
use crate::buck::types::RuleType;
use crate::buck::types::TargetLabel;
use crate::buck::types::TargetName;
use crate::changes::Changes;

/// Given the state, which .bzl files have changed, either directly or by transitive dependencies
fn changed_bzl_files<'a>(
    state: &'a Targets,
    changes: &Changes,
    track_prelude_changes: bool,
) -> HashSet<&'a CellPath> {
    let mut rdeps: HashMap<&CellPath, Vec<&CellPath>> = HashMap::new();
    let mut todo = Vec::new();
    for x in state.imports() {
        // Always track regular rule changes, but ignore buck2 prelude changes
        // unless specifically requested.
        if !track_prelude_changes && x.file.is_prelude_bzl_file() {
            continue;
        }
        if changes.contains_cell_path(&x.file) {
            todo.push(&x.file);
        }
        for y in x.imports.iter() {
            rdeps.entry(y).or_default().push(&x.file);
        }
    }

    let mut res: HashSet<_> = todo.iter().copied().collect();
    while let Some(x) = todo.pop() {
        if let Some(rdep) = rdeps.get(x) {
            for r in rdep {
                if res.insert(*r) {
                    todo.push(*r);
                }
            }
        }
    }

    res
}

fn is_changed_ci_srcs(file_deps: &[Glob], changes: &Changes) -> bool {
    if file_deps.is_empty() || changes.is_empty() {
        return false;
    }
    let glob = GlobSpec::new(file_deps);
    changes.project_paths().any(|x| glob.matches(x))
}

/// The result of `immediate_changes`.
#[derive(Debug, Default)]
pub struct GraphImpact<'a> {
    /// Targets which changed, and whose change is expected to impact
    /// things that depend on them (they changed recursively).
    recursive: Vec<&'a BuckTarget>,
    /// Targets which changed in a way that won't impact things recursively.
    /// Currently only package value changes.
    non_recursive: Vec<&'a BuckTarget>,
}

impl<'a> GraphImpact<'a> {
    pub fn len(&self) -> usize {
        self.recursive.len() + self.non_recursive.len()
    }

    pub fn iter(&'a self) -> impl Iterator<Item = &'a BuckTarget> {
        self.recursive
            .iter()
            .chain(self.non_recursive.iter())
            .copied()
    }
}

pub fn immediate_target_changes<'a>(
    base: &Targets,
    diff: &'a Targets,
    changes: &Changes,
    track_prelude_changes: bool,
) -> GraphImpact<'a> {
    // Find those targets which are different
    let old = base.targets_by_label_key();

    // Find those .bzl files that have changed, including transitive changes
    let bzl_change = changed_bzl_files(diff, changes, track_prelude_changes);

    let mut res = GraphImpact::default();
    for target in diff.targets() {
        // "hidden feature" that allows using btd to find rdeps of a "package" (directory)
        // by including directory paths in the changes input
        let change_package: bool = changes.contains_package(&target.package);
        let old_target = old.get(&target.label_key());

        // Did the hash of the target change
        let change_hash = || match old_target {
            None => true,
            Some(x) => x.hash != target.hash,
        };
        // Did the package values change
        let change_package_values = || match old_target {
            None => true,
            Some(x) => x.package_values != target.package_values,
        };
        // Did any of the sources we point at change
        let change_inputs = || target.inputs.iter().any(|x| changes.contains_cell_path(x));
        let change_ci_srcs = || is_changed_ci_srcs(&target.ci_srcs, changes);
        // Did the rule we point at change
        let change_rule =
            || !bzl_change.is_empty() && bzl_change.contains(&target.rule_type.file());

        if change_package || change_hash() || change_inputs() || change_ci_srcs() || change_rule() {
            res.recursive.push(target)
        } else if change_package_values() {
            res.non_recursive.push(target);
        }
    }

    // Sort to ensure deterministic output
    res.recursive.sort_by_key(|t| t.label_key());
    res.non_recursive.sort_by_key(|t| t.label_key());
    res
}

fn hint_applies_to(target: &BuckTarget) -> Option<(&Package, TargetName)> {
    // for hints, the name will be `foo//bar:ci_hint@baz` which means
    // we need to test `foo//bar:baz`.
    Some((
        &target.package,
        TargetName::new(target.name.as_str().strip_prefix("ci_hint@")?),
    ))
}

pub fn recursive_target_changes<'a>(
    diff: &'a Targets,
    changes: &GraphImpact<'a>,
    depth: Option<usize>,
    follow_rule_type: impl Fn(&RuleType) -> bool,
) -> Vec<Vec<&'a BuckTarget>> {
    // Just an optimisation, but saves building the reverse mapping
    if changes.recursive.is_empty() {
        let mut res = if changes.non_recursive.is_empty() {
            Vec::new()
        } else {
            vec![changes.non_recursive.clone()]
        };
        // We use a empty list sentinel to show nothing missing
        res.push(Vec::new());
        res.truncate(depth.unwrap_or(usize::MAX));
        return res;
    }

    // We expect most things will have at least one dependency, so a reasonable approximate size
    let mut rdeps: TargetMap<&BuckTarget> = TargetMap::with_capacity(diff.len_targets_upperbound());
    let mut hints: HashMap<(&Package, TargetName), TargetLabel> = HashMap::new();
    for target in diff.targets() {
        for d in target.deps.iter() {
            rdeps.insert(d, target)
        }
        for d in target.ci_deps.iter() {
            rdeps.insert_pattern(d, target);
        }
        if target.rule_type.short() == "ci_hint" {
            match hint_applies_to(target) {
                Some(dest) => {
                    hints.insert(dest, target.label());
                }
                None => warn!("`ci_hint` target has invalid name: `{}`", target.label()),
            }
        }
    }
    // We record the hints going through (while we don't have the targets to hand),
    // then fill them in later with this loop
    if !hints.is_empty() {
        for target in diff.targets() {
            if let Some(hint) = hints.remove(&(&target.package, target.name.clone())) {
                rdeps.insert(&hint, target);
                if hints.is_empty() {
                    break;
                }
            }
        }
    }

    // The code below is carefully optimised to avoid multiple lookups and reuse memory allocations.
    // We use `done` to record which elements have been queued for adding to the results, to avoid duplicates.
    // We use `todo` for things we are looping over that will become results at the end of this loop.
    // We use `next` for things we want to loop over in the next loop.
    // At the end of each loop, we add `todo` to the results and make `todo = next`.
    //
    // All non-recursive changes are already queued for adding to results, but haven't been recursively explored.
    // We record them with `done[target] = false` and add them to `next_silent` (which becomes `todo_silent`).
    // This ensures we iterate over them if reached recursively, but don't add them to results twice.

    let mut todo = changes.recursive.clone();
    let mut non_recursive_changes = changes.non_recursive.clone();

    let mut done: HashMap<TargetLabelKey, bool> = changes
        .recursive
        .iter()
        .map(|x| (x.label_key(), true))
        .chain(changes.non_recursive.iter().map(|x| (x.label_key(), false)))
        .collect();

    let mut result = Vec::new();

    let mut todo_silent = Vec::new();
    let mut next_silent = Vec::new();

    fn add_result<'a>(results: &mut Vec<Vec<&'a BuckTarget>>, mut items: Vec<&'a BuckTarget>) {
        // Sort to ensure deterministic output
        items.sort_by_key(|x| x.label_key());
        results.push(items);
    }

    for _ in 0..depth.unwrap_or(usize::MAX) {
        if todo.is_empty() && todo_silent.is_empty() {
            if !non_recursive_changes.is_empty() {
                add_result(&mut result, non_recursive_changes);
            }
            break;
        }

        let mut next = Vec::new();

        for lbl in todo.iter().chain(todo_silent.iter()) {
            if follow_rule_type(&lbl.rule_type) {
                for rdep in rdeps.get(&lbl.label()) {
                    match done.entry(rdep.label_key()) {
                        Entry::Vacant(e) => {
                            next.push(*rdep);
                            e.insert(true);
                        }
                        Entry::Occupied(mut e) => {
                            if !e.get() {
                                next_silent.push(*rdep);
                                e.insert(true);
                            }
                        }
                    }
                }
            }
        }
        if !non_recursive_changes.is_empty() {
            non_recursive_changes.extend(todo.iter());
            add_result(&mut result, mem::take(&mut non_recursive_changes));
        } else if !todo.is_empty() {
            add_result(&mut result, mem::take(&mut todo));
        }
        todo = next;

        // Do a swap so that we reuse the capacity of the buffer next time around
        mem::swap(&mut todo_silent, &mut next_silent);
        next_silent.clear();
    }

    // an empty todo list might be added to the result here, indicating to
    // the user (in Text output mode) that there are no additional levels
    add_result(&mut result, todo);
    result
}

#[cfg(test)]
mod tests {
    use itertools::Itertools;
    use td_util::prelude::*;

    use super::*;
    use crate::buck::labels::Labels;
    use crate::buck::targets::BuckImport;
    use crate::buck::targets::TargetsEntry;
    use crate::buck::types::Package;
    use crate::buck::types::PackageValues;
    use crate::buck::types::TargetHash;
    use crate::buck::types::TargetLabel;
    use crate::buck::types::TargetName;
    use crate::sapling::status::Status;

    #[test]
    fn test_immediate_changes() {
        fn target(
            pkg: &str,
            name: &str,
            inputs: &[&CellPath],
            hash: &str,
            package_values: &PackageValues,
        ) -> TargetsEntry {
            TargetsEntry::Target(BuckTarget {
                inputs: inputs.iter().map(|x| (*x).clone()).collect(),
                hash: TargetHash::new(hash),
                package_values: package_values.clone(),
                ..BuckTarget::testing(name, pkg, "prelude//rules.bzl:cxx_library")
            })
        }

        let file1 = CellPath::new("foo//bar/file1.txt");
        let file2 = CellPath::new("foo//bar/file2.txt");
        let file3 = CellPath::new("foo//bar/file3.txt");
        let file4 = CellPath::new("foo//bar/file4.txt");

        // We could get a change because the hash changes or the input changes, or both
        // Or because the target is new.
        let default_pacakge_value = PackageValues::new(&["default"], serde_json::Value::Null);
        let base = Targets::new(vec![
            target(
                "foo//bar",
                "aaa",
                &[&file1, &file2],
                "123",
                &default_pacakge_value,
            ),
            target("foo//baz", "aaa", &[&file2], "123", &default_pacakge_value),
            target("foo//bar", "bbb", &[&file3], "123", &default_pacakge_value),
            target("foo//bar", "ccc", &[&file4], "123", &default_pacakge_value),
            target("foo//bar", "ddd", &[], "123", &default_pacakge_value),
            target("foo//bar", "eee", &[], "123", &default_pacakge_value),
            target("foo//bar", "ggg", &[&file4], "123", &default_pacakge_value),
            target(
                "foo//bar",
                "zzz",
                &[&file4],
                "123",
                &PackageValues::new(&["val1"], serde_json::Value::Null),
            ),
        ]);
        let diff = Targets::new(vec![
            target(
                "foo//bar",
                "aaa",
                &[&file1, &file4],
                "123",
                &default_pacakge_value,
            ),
            target("foo//baz", "aaa", &[&file2], "123", &default_pacakge_value),
            target("foo//bar", "bbb", &[&file3], "123", &default_pacakge_value),
            target("foo//bar", "ccc", &[&file4], "123", &default_pacakge_value),
            target("foo//bar", "fff", &[], "123", &default_pacakge_value),
            target("foo//bar", "ggg", &[&file4], "321", &default_pacakge_value),
            // only package value changed
            target(
                "foo//bar",
                "zzz",
                &[&file4],
                "123",
                &PackageValues::new(&["val2"], serde_json::Value::Null),
            ),
        ]);
        let res = immediate_target_changes(
            &base,
            &diff,
            &Changes::testing(&[
                Status::Modified(file1),
                Status::Added(file2),
                Status::Removed(file3),
            ]),
            false,
        );
        let recursive = res.recursive.map(|x| x.label().to_string());
        let non_recursive = res.non_recursive.map(|x| x.label().to_string());
        assert_eq!(
            recursive.map(|x| x.as_str()),
            &[
                "foo//bar:aaa",
                "foo//bar:bbb",
                "foo//bar:fff",
                "foo//bar:ggg",
                "foo//baz:aaa",
            ]
        );
        assert_eq!(non_recursive.map(|x| x.as_str()), &["foo//bar:zzz",]);
    }

    #[test]
    fn test_package_changes() {
        fn target(pkg: &str, name: &str, inputs: &[&CellPath], hash: &str) -> TargetsEntry {
            TargetsEntry::Target(BuckTarget {
                inputs: inputs.iter().map(|x| (*x).clone()).collect(),
                hash: TargetHash::new(hash),
                ..BuckTarget::testing(name, pkg, "prelude//rules.bzl:cxx_library")
            })
        }

        let file1 = CellPath::new("foo//bar/file1.txt");
        let file2 = CellPath::new("foo//bar/file2.txt");
        let package = CellPath::new("foo//bar");

        let base = Targets::new(vec![
            target("foo//bar", "aaa", &[&file1, &file2], "123"),
            target("foo//baz", "aaa", &[&file2], "123"),
            target("foo//bar", "bbb", &[], "123"),
        ]);
        let res = immediate_target_changes(
            &base,
            &base,
            &Changes::testing(&[Status::Modified(package)]),
            false,
        );
        let mut res = res.recursive.map(|x| x.label().to_string());
        res.sort();
        let res = res.map(|x| x.as_str());
        assert_eq!(&res, &["foo//bar:aaa", "foo//bar:bbb",]);
    }
    #[test]
    fn test_recursive_changes_non_recursive_only() {
        fn target(name: &str, deps: &[&str], package_values: &PackageValues) -> TargetsEntry {
            let pkg = Package::new("foo//");
            TargetsEntry::Target(BuckTarget {
                deps: deps.iter().map(|x| pkg.join(&TargetName::new(x))).collect(),
                package_values: package_values.clone(),
                ..BuckTarget::testing(name, pkg.as_str(), "prelude//rules.bzl:cxx_library")
            })
        }

        let diff = Targets::new(vec![target(
            "a",
            &[],
            &PackageValues::new(&["val"], serde_json::Value::Null),
        )]);

        let changes = GraphImpact {
            recursive: Vec::new(),
            non_recursive: vec![diff.targets().next().unwrap()],
        };
        let res = recursive_target_changes(&diff, &changes, Some(2), |_| true);
        let res = res.map(|xs| {
            let mut xs = xs.map(|x| x.name.as_str());
            xs.sort();
            xs
        });
        assert_eq!(res, vec![vec!["a"], vec![]]);
    }

    #[test]
    fn test_recursive_changes_with_package_values_only_changes() {
        fn target(name: &str, deps: &[&str], package_values: &PackageValues) -> TargetsEntry {
            let pkg = Package::new("foo//");
            TargetsEntry::Target(BuckTarget {
                deps: deps.iter().map(|x| pkg.join(&TargetName::new(x))).collect(),
                package_values: package_values.clone(),
                ..BuckTarget::testing(name, pkg.as_str(), "prelude//rules.bzl:cxx_library")
            })
        }

        let diff = Targets::new(vec![
            target(
                "a",
                &[],
                &PackageValues::new(&["val"], serde_json::Value::Null),
            ),
            target(
                "b",
                &["a"],
                &PackageValues::new(&["non_recursive_change"], serde_json::Value::Null),
            ),
            target(
                "c",
                &["b"],
                &PackageValues::new(&["val"], serde_json::Value::Null),
            ),
        ]);

        let changes = GraphImpact {
            recursive: vec![diff.targets().next().unwrap()],
            non_recursive: vec![diff.targets().nth(1).unwrap()],
        };
        let res = recursive_target_changes(&diff, &changes, Some(2), |_| true);
        let res = res.map(|xs| {
            let mut xs = xs.map(|x| x.name.as_str());
            xs.sort();
            xs
        });
        assert_eq!(res, vec![vec!["a", "b"], vec!["c"]]);
    }

    #[test]
    fn test_recursive_changes() {
        // We should be able to deal with cycles, and pieces that aren't on the graph
        fn target(name: &str, deps: &[&str]) -> TargetsEntry {
            let pkg = Package::new("foo//");
            TargetsEntry::Target(BuckTarget {
                deps: deps.iter().map(|x| pkg.join(&TargetName::new(x))).collect(),
                ..BuckTarget::testing(name, pkg.as_str(), "prelude//rules.bzl:cxx_library")
            })
        }
        let diff = Targets::new(vec![
            target("a", &["1"]),
            target("1", &[]),
            target("b", &["a"]),
            target("c", &["a", "d"]),
            target("d", &["b", "c"]),
            target("e", &["d", "b"]),
            target("f", &["e"]),
            target("g", &["f", "1"]),
            target("z", &[]),
            target("package_value_only", &[]),
        ]);

        let changes = GraphImpact {
            recursive: vec![diff.targets().next().unwrap()],
            non_recursive: Vec::new(),
        };
        let res = recursive_target_changes(&diff, &changes, Some(3), |_| true);
        let res = res.map(|xs| {
            let mut xs = xs.map(|x| x.name.as_str());
            xs.sort();
            xs
        });
        assert_eq!(
            res,
            vec![vec!["a"], vec!["b", "c"], vec!["d", "e"], vec!["f"],]
        );
    }

    #[test]
    fn test_recursive_changes_returns_unique_targets() {
        fn target(name: &str, deps: &[&str]) -> TargetsEntry {
            let pkg = Package::new("foo//");
            TargetsEntry::Target(BuckTarget {
                deps: deps.iter().map(|x| pkg.join(&TargetName::new(x))).collect(),
                ..BuckTarget::testing(name, pkg.as_str(), "prelude//rules.bzl:cxx_library")
            })
        }
        let diff = Targets::new(vec![
            target("a", &["1"]),
            target("b", &["a", "c"]),
            target("1", &[]),
            target("c", &["a"]),
            target("d", &["a", "c"]),
        ]);

        let changes = GraphImpact {
            recursive: diff.targets().take(2).collect_vec(),
            non_recursive: Vec::new(),
        };
        let res = recursive_target_changes(&diff, &changes, None, |_| true);
        let res = res.map(|xs| xs.map(|x| x.name.as_str()));
        assert_eq!(res, vec![vec!["a", "b"], vec!["c", "d"], vec![]]);
    }

    #[test]
    fn test_prelude_rule_changes() {
        // prelude.bzl imports rules.bzl which imports foo.bzl
        let targets = Targets::new(vec![
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("prelude//prelude.bzl"),
                imports: Box::new([CellPath::new("prelude//rules.bzl")]),
                package: None,
            }),
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("prelude//rules.bzl"),
                imports: Box::new([CellPath::new("prelude//utils.bzl")]),
                package: None,
            }),
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("prelude//utils.bzl"),
                imports: Box::new([]),
                package: None,
            }),
            TargetsEntry::Target(BuckTarget::testing(
                "foo",
                "code//bar",
                "prelude//rules.bzl:genrule",
            )),
        ]);
        let check = |file, check, expect: usize| {
            assert_eq!(
                immediate_target_changes(
                    &targets,
                    &targets,
                    &Changes::testing(&[Status::Modified(CellPath::new(file))]),
                    check
                )
                .len(),
                expect
            )
        };
        check("prelude//rules.bzl", false, 0);
        check("prelude//rules.bzl", true, 1);
        check("prelude//utils.bzl", true, 1);
        check("prelude//prelude.bzl", true, 0);
    }

    #[test]
    fn test_non_prelude_rule_changes() {
        // test.bzl imports my_rules.bzl which imports prelude//rules.bzl
        let targets = Targets::new(vec![
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("fbcode//test.bzl"),
                imports: Box::new([CellPath::new("fbcode//my_rules.bzl")]),
                package: None,
            }),
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("fbcode//my_rules.bzl"),
                imports: Box::new([CellPath::new("prelude//rules.bzl")]),
                package: None,
            }),
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("prelude//rules.bzl"),
                imports: Box::new([CellPath::new("prelude//utils.bzl")]),
                package: None,
            }),
            TargetsEntry::Import(BuckImport {
                file: CellPath::new("prelude//utils.bzl"),
                imports: Box::new([]),
                package: None,
            }),
            TargetsEntry::Target(BuckTarget::testing(
                "foo",
                "fbcode//bar",
                "fbcode//my_rules.bzl:my_rule",
            )),
            TargetsEntry::Target(BuckTarget::testing(
                "baz",
                "fbcode//bar",
                "prelude//rules.bzl:genrule",
            )),
        ]);
        let check = |file, check, expect: usize| {
            assert_eq!(
                immediate_target_changes(
                    &targets,
                    &targets,
                    &Changes::testing(&[Status::Modified(CellPath::new(file))]),
                    check
                )
                .len(),
                expect
            )
        };
        // Changes to non-prelude rules still tracks rule changes.
        check("fbcode//my_rules.bzl", false, 1);
        check("fbcode//my_rules.bzl", true, 1);
        // Changes from prelude are only tracked if the boolean is set.
        check("prelude//rules.bzl", false, 0);
        check("prelude//rules.bzl", true, 2);
        check("prelude//utils.bzl", false, 0);
        check("prelude//utils.bzl", true, 2);
    }

    #[test]
    fn test_file_deps() {
        // prelude.bzl imports rules.bzl which imports foo.bzl
        let targets = Targets::new(vec![TargetsEntry::Target(BuckTarget {
            ci_srcs: Box::new([Glob::new("test/*.txt")]),
            ..BuckTarget::testing("foo", "code//bar", "prelude//rules.bzl:genrule")
        })]);
        let check = |file, expect: usize| {
            assert_eq!(
                immediate_target_changes(
                    &targets,
                    &targets,
                    &Changes::testing(&[Status::Modified(CellPath::new(&format!("root//{file}")))]),
                    false
                )
                .len(),
                expect
            )
        };
        check("prelude/rules.bzl", 0);
        check("test/foo.java", 0);
        check("test/foo.txt", 1);
    }

    #[test]
    fn test_package_values() {
        // prelude.bzl imports rules.bzl which imports foo.bzl
        let before = Targets::new(vec![TargetsEntry::Target(BuckTarget::testing(
            "foo",
            "code//bar",
            "prelude//rules.bzl:genrule",
        ))]);
        let after = Targets::new(vec![TargetsEntry::Target(BuckTarget {
            package_values: PackageValues {
                labels: Labels::new(&["foo"]),
                cfg_modifiers: serde_json::Value::Null,
            },
            ..BuckTarget::testing("foo", "code//bar", "prelude//rules.bzl:genrule")
        })]);
        // The hash of the target doesn't change, but the package.value does
        assert_eq!(
            immediate_target_changes(&before, &after, &Changes::testing(&[]), false).len(),
            1
        );
    }

    #[test]
    fn test_graph_with_node_cycles() {
        let src = CellPath::new("foo//src.txt");

        // You can get a graph which has cycles because the uquery graph has cycles, but cquery doesn't.
        // Or because the graph is broken but Buck2 won't see that with streaming targets.
        let targets = Targets::new(vec![
            TargetsEntry::Target(BuckTarget {
                deps: Box::new([TargetLabel::new("foo//:b")]),
                inputs: Box::new([src.clone()]),
                ..BuckTarget::testing("a", "foo//", "")
            }),
            TargetsEntry::Target(BuckTarget {
                deps: Box::new([TargetLabel::new("foo//:a")]),
                ..BuckTarget::testing("b", "foo//", "")
            }),
        ]);
        let changes = Changes::testing(&[Status::Modified(src)]);
        let mut impact = immediate_target_changes(&targets, &targets, &changes, false);
        assert_eq!(impact.recursive.len(), 1);

        assert_eq!(
            recursive_target_changes(&targets, &impact, None, |_| true)
                .iter()
                .flatten()
                .count(),
            2
        );
        impact.recursive.push(targets.targets().nth(1).unwrap());
        assert_eq!(
            recursive_target_changes(&targets, &impact, None, |_| true)
                .iter()
                .flatten()
                .count(),
            2
        );
    }

    #[test]
    fn test_recursive_changes_hint() {
        // We should be able to deal with cycles, and pieces that aren't on the graph
        let diff = Targets::new(vec![
            TargetsEntry::Target(BuckTarget {
                ..BuckTarget::testing(
                    "ci_hint@baz",
                    "foo//bar",
                    "fbcode//target_determinator/macros/rules/ci_hint.bzl:ci_hint",
                )
            }),
            TargetsEntry::Target(BuckTarget {
                ..BuckTarget::testing("baz", "foo//bar", "prelude//rules.bzl:cxx_library")
            }),
        ]);

        let changes = GraphImpact {
            recursive: vec![diff.targets().next().unwrap()],
            non_recursive: Vec::new(),
        };
        let res = recursive_target_changes(&diff, &changes, Some(3), |_| true);
        assert_eq!(res[0].len(), 1);
        assert_eq!(res[1].len(), 1);
        assert_eq!(res[1][0].name, TargetName::new("baz"));
        assert_eq!(res.iter().flatten().count(), 2);
    }
}
