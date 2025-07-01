/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::HashSet;

use itertools::Itertools;

use crate::buck::targets::BuckTarget;
use crate::buck::targets::Targets;
use crate::buck::types::RuleType;
use crate::buck::types::TargetLabelKeyRef;
use crate::changes::Changes;
use crate::diff::ImpactTraceData;
use crate::diff::immediate_target_changes;
use crate::diff::recursive_target_changes;

fn cxx_rule_type(typ: &RuleType) -> bool {
    let short = typ.short();
    short == "cxx_library" || short == "cxx_executable"
}

/// Compute the things that have changed that are interesting to Glean.
///
/// If the cxx_library/cxx_executable rules themselves change, rebuild everything.
/// If the .h file changes, transitively impact everything.
/// If the other input files change, only impact directly enclosing cxx_library/cxx_executable.
pub fn glean_changes<'a>(
    base: &'a Targets,
    diff: &'a Targets,
    changes: &Changes,
    depth: Option<usize>,
) -> Vec<Vec<(&'a BuckTarget, ImpactTraceData)>> {
    let header = immediate_target_changes(
        base,
        diff,
        &changes.filter_by_extension(|x| x == Some("h")),
        true,
    );
    let header_rec = recursive_target_changes(diff, changes, &header, depth, |_| true);
    let other = immediate_target_changes(base, diff, changes, true);
    let other_rec = recursive_target_changes(diff, changes, &other, depth, |x| !cxx_rule_type(x));
    merge(header_rec, other_rec)
}

fn merge<'a>(
    a: Vec<Vec<(&'a BuckTarget, ImpactTraceData)>>,
    b: Vec<Vec<(&'a BuckTarget, ImpactTraceData)>>,
) -> Vec<Vec<(&'a BuckTarget, ImpactTraceData)>> {
    let mut seen: HashSet<TargetLabelKeyRef> = HashSet::new();
    let mut res = Vec::new();
    for layer in a.into_iter().zip_longest(b) {
        let mut res1 = Vec::new();
        let (left, right) = layer.or_default();

        for (item, reason) in left.into_iter().chain(right) {
            if seen.insert(item.label_key()) && cxx_rule_type(&item.rule_type) {
                res1.push((item, reason))
            }
        }
        if !res1.is_empty() {
            res1.sort_by_key(|(x, _)| x.label_key());
            res.push(res1)
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use td_util::prelude::*;

    use super::*;
    use crate::buck::targets::TargetsEntry;
    use crate::buck::types::CellPath;
    use crate::buck::types::TargetLabel;
    use crate::sapling::status::Status;

    #[test]
    fn test_glean() {
        let cxx_lib = "prelude//rules.bzl:cxx_library";
        let cxx_exe = "prelude//rules.bzl:cxx_executable";
        let other = "prelude//rules.bzl:other";

        let targets = Targets::new(vec![
            TargetsEntry::Target(BuckTarget {
                inputs: Box::new([CellPath::new("root//test.h")]),
                ..BuckTarget::testing("lib1", "root//", cxx_lib)
            }),
            TargetsEntry::Target(BuckTarget {
                inputs: Box::new([CellPath::new("root//test.cpp")]),
                ..BuckTarget::testing("exporter", "root//", other)
            }),
            TargetsEntry::Target(BuckTarget {
                deps: Box::new([TargetLabel::new("root//:exporter")]),
                ..BuckTarget::testing("lib2", "root//", cxx_lib)
            }),
            TargetsEntry::Target(BuckTarget {
                deps: Box::new([
                    TargetLabel::new("root//:lib1"),
                    TargetLabel::new("root//:lib2"),
                ]),
                ..BuckTarget::testing("bin1", "root//", cxx_exe)
            }),
            TargetsEntry::Target(BuckTarget {
                deps: Box::new([TargetLabel::new("root//:lib2")]),
                ..BuckTarget::testing("bin2", "root//", cxx_exe)
            }),
            TargetsEntry::Target(BuckTarget {
                inputs: Box::new([CellPath::new("root//test.cpp")]),
                ..BuckTarget::testing("user", "root//", other)
            }),
        ]);

        let res = glean_changes(
            &targets,
            &targets,
            &Changes::testing(&[
                Status::Modified(CellPath::new("root//test.cpp")),
                Status::Modified(CellPath::new("root//test.h")),
            ]),
            None,
        );
        let mut res = res.concat().map(|(x, _)| x.label());
        res.sort();
        let want = vec![
            TargetLabel::new("root//:bin1"),
            TargetLabel::new("root//:lib1"),
            TargetLabel::new("root//:lib2"),
        ];
        assert_eq!(res, want);
    }
}
