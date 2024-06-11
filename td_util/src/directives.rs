/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Parsing directives from skycastle

use crate::project::TdProject;

pub fn get_app_specific_build_directives(directives: &Option<Vec<String>>) -> Option<Vec<String>> {
    directives.as_ref().map(|directives| {
        directives
            .iter()
            .filter_map(|directive| {
                if directive.starts_with("@build[") && directive.ends_with(']') {
                    Some(
                        directive[7..directive.len() - 1]
                            .split(',')
                            .map(|s| s.to_string())
                            .collect::<Vec<String>>(),
                    )
                } else {
                    None
                }
            })
            .flatten()
            .collect::<Vec<String>>()
    })
}

pub fn app_specific_build_directives_matches_name(
    app_specific_build_directives: &Option<Vec<String>>,
    name: &String,
    exactly: bool,
    project: TdProject,
) -> bool {
    app_specific_build_directives
        .as_ref()
        .map_or(false, |app_specific_build_directives| {
            app_specific_build_directives.iter().any(|directive| {
                if exactly && project != TdProject::Fbobjc {
                    name == directive
                } else {
                    name.starts_with(directive) || name.ends_with(directive)
                }
            })
        })
}

pub fn should_build_all_fbobjc(directives: &Option<Vec<String>>, project: TdProject) -> bool {
    return directives
        .iter()
        .flatten()
        .any(|build_directive| build_directive == "#buildall-fbobjc")
        && project == TdProject::Fbobjc;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_get_app_specific_build_directives() {
        let directives = Some(vec![
            "@build[directive1,directive2]".to_string(),
            "@build[directive3]".to_string(),
            "not a directive".to_string(),
        ]);
        let result = get_app_specific_build_directives(&directives);
        assert_eq!(
            result,
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
        let result = get_app_specific_build_directives(&directives);
        assert_eq!(result, None);
    }
    #[test]
    fn test_app_specific_build_directives_contains_name() {
        let app_specific_build_directives = Some(vec![
            "directive1".to_string(),
            "directive2".to_string(),
            "directive3".to_string(),
        ]);
        assert!(app_specific_build_directives_matches_name(
            &app_specific_build_directives,
            &"directive1".to_string(),
            true,
            TdProject::Fbandroid
        ));
        assert!(!app_specific_build_directives_matches_name(
            &app_specific_build_directives,
            &"directive4".to_string(),
            true,
            TdProject::Fbandroid
        ));
    }
    #[test]
    fn test_app_specific_build_directives_contains_name_none() {
        let app_specific_build_directives = None;
        assert!(!app_specific_build_directives_matches_name(
            &app_specific_build_directives,
            &"directive1".to_string(),
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
            &app_specific_build_directives,
            &"directive1234".to_string(),
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
            &fbobjc_app_specific_build_directives,
            &"barcelona-distribution-iphoneos-release-buck2".to_string(),
            true,
            TdProject::Fbobjc
        ));
        assert!(app_specific_build_directives_matches_name(
            &fbobjc_app_specific_build_directives,
            &"igios-distribution-iphoneos-production-buck2".to_string(),
            true,
            TdProject::Fbobjc
        ));
        assert!(!app_specific_build_directives_matches_name(
            &fbobjc_app_specific_build_directives,
            &"igios-iphonesimulator-local-buck2".to_string(),
            true,
            TdProject::Fbobjc
        ));
        let fbandroid_app_specific_build_directives =
            Some(vec!["fb4a-debug".to_string(), "fb4a-release".to_string()]);
        assert!(app_specific_build_directives_matches_name(
            &fbandroid_app_specific_build_directives,
            &"automation-fb4a-debug".to_string(),
            false,
            TdProject::Fbandroid
        ));
        assert!(app_specific_build_directives_matches_name(
            &fbandroid_app_specific_build_directives,
            &"automation-fb4a-release".to_string(),
            false,
            TdProject::Fbandroid
        ));
    }
}
