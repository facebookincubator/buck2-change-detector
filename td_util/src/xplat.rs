/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Utilities to work with xplat

use std::collections::HashMap;

use crate::project::TdProject;

const FBANDROID_TEST_SELECTION_CONFIG_JOB_METADATA_KEY: &str = "fbandroid.test_selection_config";
const FBOBJC_TEST_SELECTION_CONFIG_JOB_METADATA_KEY: &str = "fbobjc.test_selection_config";

pub fn unpack_project_metadata(
    project: TdProject,
    job_metadata: &[(String, String)],
) -> Vec<(String, String)> {
    let unpack_json_metadata = |metadata_key: &str| -> Vec<(String, String)> {
        job_metadata
            .iter()
            .find(|m| m.0 == metadata_key)
            .map_or(HashMap::new(), |m| {
                serde_json::from_str::<HashMap<String, String>>(&m.1).unwrap_or(HashMap::new())
            })
            .into_iter()
            .collect()
    };

    match project {
        TdProject::Fbandroid => {
            let mut ret = unpack_json_metadata(FBANDROID_TEST_SELECTION_CONFIG_JOB_METADATA_KEY);
            ret.extend_from_slice(job_metadata);
            ret
        }
        TdProject::Fbobjc => {
            let mut ret = unpack_json_metadata(FBOBJC_TEST_SELECTION_CONFIG_JOB_METADATA_KEY);
            ret.extend_from_slice(job_metadata);
            ret
        }
        _ => job_metadata.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unpack_project_metadata() {
        let job_metadata: Vec<(String, String)> = vec![
            (
                "fbandroid.test_selection_config".into(),
                r#"{"test.abc": "1", "test.def": "2"}"#.into(),
            ),
            ("foo".into(), "bar".into()),
            (
                "fbobjc.test_selection_config".into(),
                r#"{"test.xyz": "1"}"#.into(),
            ),
        ];

        assert_eq!(
            unpack_project_metadata(TdProject::Fbandroid, &job_metadata),
            [
                vec![
                    ("test.abc".into(), "1".into()),
                    ("test.def".into(), "2".into())
                ],
                job_metadata.clone(),
            ]
            .concat()
        );
        assert_eq!(
            unpack_project_metadata(TdProject::Fbobjc, &job_metadata),
            [vec![("test.xyz".into(), "1".into()),], job_metadata.clone(),].concat()
        );
    }
}
