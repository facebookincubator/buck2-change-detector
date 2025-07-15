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

use td_util_buck::targets::BuckTarget;
use td_util_buck::targets::Targets;
use td_util_buck::types::TargetLabel;
use td_util_buck::types::TargetLabelKeyRef;

// Currently, this function doesn't support progagating 'uses_sudo' label for target patterns.
// We can possibly live with this version until a use case found.
pub fn requires_sudo_recursively(targets: &Targets) -> HashSet<TargetLabelKeyRef> {
    let mut rdeps: HashMap<&TargetLabel, Vec<&BuckTarget>> =
        HashMap::with_capacity(targets.len_targets_upperbound());
    let mut todo: Vec<&BuckTarget> = Vec::new();
    let mut sudos: HashSet<TargetLabelKeyRef> = HashSet::new();

    for target in targets.targets() {
        for d in target.deps.iter() {
            rdeps.entry(d).or_default().push(target);
        }
        if target.labels.contains("uses_sudo") {
            todo.push(target);
            sudos.insert(target.label_key());
        }
    }

    while let Some(lbl) = todo.pop() {
        if let Some(parents) = rdeps.get(&lbl.label()) {
            for parent in parents {
                if sudos.insert(parent.label_key()) {
                    todo.push(*parent)
                }
            }
        }
    }

    sudos
}

#[cfg(test)]
mod tests {
    use td_util_buck::labels::Labels;
    use td_util_buck::targets::TargetsEntry;
    use td_util_buck::types::Package;
    use td_util_buck::types::TargetName;

    use super::*;

    #[test]
    fn test_requires_sudo_recursively() {
        fn target(name: &str, deps: &[&str], uses_sudo: bool) -> TargetsEntry {
            let pkg = Package::new("foo//");
            let labels = if uses_sudo {
                Labels::new(&["uses_sudo"])
            } else {
                Labels::default()
            };
            TargetsEntry::Target(BuckTarget {
                deps: deps.iter().map(|x| pkg.join(&TargetName::new(x))).collect(),
                labels,
                ..BuckTarget::testing(name, pkg.as_str(), "prelude//rules.bzl:cxx_library")
            })
        }
        let targets = Targets::new(vec![
            // the leaf node requires sudo
            target("1", &[], true),
            target("1a", &["1"], false),
            target("1b", &["1a"], false),
            // middle node requires sudo
            target("2", &[], false),
            target("2a", &["2"], true),
            target("2b", &["2a"], false),
            // root node requires sudo
            target("3", &[], false),
            target("3a", &["3"], false),
            target("3b", &["3a"], true),
            // no sudo
            target("4", &[], false),
            target("4a", &["4"], false),
            target("4b", &["4a"], false),
            // one of the dependencies requies sudo
            target("5", &[], false),
            target("5a", &["5"], false),
            target("5b", &[], true),
            target("5c", &["5a", "5b"], false),
            // multiple visits that creates early return
            target("6", &[], true),
            target("6a", &["6"], true),
            target("6b", &["6a"], false),
        ]);
        let targets_by_key = targets.targets_by_label_key();
        let mut res = requires_sudo_recursively(&targets)
            .iter()
            .map(|x| targets_by_key.get(x).unwrap().name.as_str())
            .collect::<Vec<_>>();
        res.sort();

        assert_eq!(
            res,
            vec![
                "1", "1a", "1b", "2a", "2b", "3b", "5b", "5c", "6", "6a", "6b"
            ]
        );
    }
}
