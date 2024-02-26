/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Equivalent to the Buck2 `glob` to the greatest extent possible.

use glob::MatchOptions;
use glob::Pattern;

use crate::buck::types::Glob;
use crate::buck::types::ProjectRelativePath;

pub struct GlobSpec(Vec<Pattern>);

impl GlobSpec {
    pub fn new(xs: &[Glob]) -> Self {
        // We just throw away any inaccurate globs for now, and rely on the macro layer spotting them.
        // We probably want a lint pass sooner or later.
        Self(xs.iter().flat_map(|x| Pattern::new(x.as_str())).collect())
    }

    pub fn matches(&self, path: &ProjectRelativePath) -> bool {
        let options = MatchOptions {
            require_literal_separator: true,
            require_literal_leading_dot: true,
            // Buck2 is currently case insensitive, but they want to fix that, so we should be more picky
            case_sensitive: true,
        };
        self.0
            .iter()
            .any(|x| x.matches_with(path.as_str(), options))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob() {
        fn f(glob: &str, path: &str, res: bool) {
            assert_eq!(
                res,
                GlobSpec::new(&[Glob::new(glob)]).matches(&ProjectRelativePath::new(path)),
                "With {glob:?} and {path:?}"
            )
        }

        f("abc*", "abcxyz", true);
        f("abc*", "abcxyz/bar", false);
        f("foo/*", "foo/abc", true);
        f("foo/*", "foo/abc/bar", false);
        f("**/*.java", "foo/bar/baz/me.java", true);
        f("**/*.java", "foo/bar/baz/me.jar", false);
        f("simple", "simple", true);
        f("foo/bar/**", "foo/bar/baz/qux.txt", true);
        f("foo/bar/**", "foo/bar/magic", true);
        f("foo/bar/**", "foo/bard", false);
        f("foo/bar/**", "elsewhere", false);
    }
}
