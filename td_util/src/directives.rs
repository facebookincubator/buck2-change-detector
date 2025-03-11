/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Parsing directives from skycastle

use std::collections::HashSet;

use crate::project::TdProject;

pub const BUILD_ALL_DIRECTIVE: &str = "#buildall";
pub const BUILD_ALL_FBANDROID_DIRECTIVE: &str = "#buildall-fbandroid";
pub const BUILD_ALL_FBOBJC_DIRECTIVE: &str = "#buildall-fbobjc";
const BUILD_RULE_TYPE_DIRECTIVE_PREFIX: &str = "#build_rule_type[";
const RUNWAY_TEST_TAG_DIRECTIVE_PREFIX: &str = "#runway_test_tag[";

pub fn get_app_specific_build_directives(directives: Option<&[String]>) -> Option<Vec<String>> {
    Some(
        directives?
            .iter()
            .filter_map(|directive| directive.strip_prefix("@build[")?.strip_suffix(']'))
            .filter(|x| !x.is_empty())
            .flat_map(|directive| directive.split(','))
            .map(ToOwned::to_owned)
            .collect(),
    )
}

pub fn app_specific_build_directives_matches_name(
    app_specific_build_directives: Option<&[String]>,
    name: &str,
    exactly: bool,
    project: TdProject,
) -> bool {
    app_specific_build_directives.map_or(false, |app_specific_build_directives| {
        app_specific_build_directives.iter().any(|directive| {
            if exactly && project != TdProject::Fbobjc {
                name == directive
            } else {
                name.starts_with(directive) || name.ends_with(directive)
            }
        })
    })
}

pub fn should_build_all(directives: Option<&[String]>) -> bool {
    let Some(directives) = directives else {
        return false;
    };
    directives.iter().any(|x| x == BUILD_ALL_DIRECTIVE)
}

pub fn should_skip_relates(directives: Option<&[String]>, target: &str) -> bool {
    let Some(directives) = directives else {
        return false;
    };
    directives
        .iter()
        .any(|x| x == "#skip_relates_all" || *x == format!("#skip_relates[{}]", target))
}

pub fn extract_runway_test_tag(directives: Option<&[String]>) -> HashSet<String> {
    get_directive_hash_set_values(directives, RUNWAY_TEST_TAG_DIRECTIVE_PREFIX)
}

pub fn get_build_rule_types(directives: Option<&[String]>) -> HashSet<String> {
    get_directive_hash_set_values(directives, BUILD_RULE_TYPE_DIRECTIVE_PREFIX)
}

fn get_directive_hash_set_values(
    directives: Option<&[String]>,
    directive_prefix: &str,
) -> HashSet<String> {
    let Some(directives) = directives else {
        return HashSet::new();
    };
    directives
        .iter()
        .filter_map(|directive| directive.strip_prefix(directive_prefix)?.strip_suffix(']'))
        .filter(|x| !x.is_empty())
        .flat_map(|rule_types| rule_types.split(','))
        .map(ToOwned::to_owned)
        .collect()
}

pub fn should_build_all_fbobjc(directives: Option<&[String]>, project: TdProject) -> bool {
    project == TdProject::Fbobjc
        && directives
            .into_iter()
            .flatten()
            .any(|build_directive| build_directive == BUILD_ALL_FBOBJC_DIRECTIVE)
}

pub fn should_build_all_fbandroid(directives: Option<&[String]>, project: TdProject) -> bool {
    project == TdProject::Fbandroid
        && directives
            .into_iter()
            .flatten()
            .any(|build_directive| build_directive == BUILD_ALL_FBANDROID_DIRECTIVE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_build_rule_types() {
        assert_eq!(
            HashSet::from(["cxx_library".to_owned(), "rust_library".to_owned()]),
            get_build_rule_types(Some(&[
                "#build_rule_type[cxx_library,rust_library]".to_owned(),
                "#build_rule_type[cxx_library]".to_owned()
            ]))
        );

        assert_eq!(
            HashSet::new(),
            get_build_rule_types(Some(&["#buildall".to_owned()])),
        );

        assert_eq!(
            HashSet::new(),
            get_build_rule_types(Some(&["#build_rule_type[]".to_owned()])),
        );
    }

    #[test]
    fn test_extract_runway_test_tag() {
        assert_eq!(
            HashSet::new(),
            extract_runway_test_tag(Some(&["#other_directive".to_owned()]))
        );

        assert_eq!(
            HashSet::from(["push-blocking".to_owned(), "tenant".to_owned()]),
            extract_runway_test_tag(Some(&[
                "#runway_test_tag[push-blocking,tenant,push-blocking]".to_owned(),
                "#other_directive[]".to_owned()
            ]))
        );
    }

    #[test]
    fn test_malformed_runway_test_tag() {
        assert_eq!(
            HashSet::new(),
            extract_runway_test_tag(Some(&["#runway_test_tag[]".to_owned(),]))
        );

        assert_eq!(
            HashSet::new(),
            extract_runway_test_tag(Some(&["#runway_test_tag[".to_owned()]))
        );

        assert_eq!(
            HashSet::from(["two:tag".to_owned()]),
            extract_runway_test_tag(Some(&["#runway_test_tag[two:tag]".to_owned()]))
        );

        assert_eq!(
            HashSet::from(["tag".to_owned(), "2:tag".to_owned()]),
            extract_runway_test_tag(Some(&["#runway_test_tag[tag,2:tag]".to_owned()]))
        );
    }

    #[test]
    fn test_get_app_specific_build_directives() {
        let directives = Some(vec![
            "@build[directive1,directive2]".to_string(),
            "@build[directive3]".to_string(),
            "not a directive".to_string(),
        ]);
        assert_eq!(
            get_app_specific_build_directives(directives.as_deref()),
            Some(vec![
                "directive1".to_string(),
                "directive2".to_string(),
                "directive3".to_string(),
            ])
        );
    }

    #[test]
    fn test_get_app_specific_build_directives_none() {
        let directives = None;
        assert_eq!(get_app_specific_build_directives(directives), None);
    }

    #[test]
    fn test_get_app_specific_build_directives_empty() {
        let directives = Some(vec!["@build[]".to_string()]);
        assert_eq!(
            get_app_specific_build_directives(directives.as_deref()),
            Some(vec![]),
        );
    }

    #[test]
    fn test_app_specific_build_directives_contains_name() {
        let app_specific_build_directives = Some(vec![
            "directive1".to_string(),
            "directive2".to_string(),
            "directive3".to_string(),
        ]);
        assert!(app_specific_build_directives_matches_name(
            app_specific_build_directives.as_deref(),
            "directive1",
            true,
            TdProject::Fbandroid
        ));
        assert!(!app_specific_build_directives_matches_name(
            app_specific_build_directives.as_deref(),
            "directive4",
            true,
            TdProject::Fbandroid
        ));
    }
    #[test]
    fn test_app_specific_build_directives_contains_name_none() {
        let app_specific_build_directives = None;
        assert!(!app_specific_build_directives_matches_name(
            app_specific_build_directives,
            "directive1",
            true,
            TdProject::Fbandroid
        ));
    }

    #[test]
    fn test_app_specific_build_directives_matches_partially() {
        let app_specific_build_directives = Some(vec![
            "directive1".to_string(),
            "directive2".to_string(),
            "directive3".to_string(),
        ]);
        assert!(app_specific_build_directives_matches_name(
            app_specific_build_directives.as_deref(),
            "directive1234",
            false,
            TdProject::Fbandroid
        ));
    }

    #[test]
    fn test_app_specific_build_directives_matches_suffix() {
        let fbobjc_app_specific_build_directives = Some(vec![
            "-iphoneos-release-buck2".to_string(),
            "-iphoneos-production-buck2".to_string(),
        ]);
        assert!(app_specific_build_directives_matches_name(
            fbobjc_app_specific_build_directives.as_deref(),
            "barcelona-distribution-iphoneos-release-buck2",
            true,
            TdProject::Fbobjc
        ));
        assert!(app_specific_build_directives_matches_name(
            fbobjc_app_specific_build_directives.as_deref(),
            "igios-distribution-iphoneos-production-buck2",
            true,
            TdProject::Fbobjc
        ));
        assert!(!app_specific_build_directives_matches_name(
            fbobjc_app_specific_build_directives.as_deref(),
            "igios-iphonesimulator-local-buck2",
            true,
            TdProject::Fbobjc
        ));
        let fbandroid_app_specific_build_directives =
            Some(vec!["fb4a-debug".to_string(), "fb4a-release".to_string()]);
        assert!(app_specific_build_directives_matches_name(
            fbandroid_app_specific_build_directives.as_deref(),
            "automation-fb4a-debug",
            false,
            TdProject::Fbandroid
        ));
        assert!(app_specific_build_directives_matches_name(
            fbandroid_app_specific_build_directives.as_deref(),
            "automation-fb4a-release",
            false,
            TdProject::Fbandroid
        ));
    }
}
