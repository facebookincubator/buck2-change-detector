/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use serde::Serialize;

use crate::supertd_events;

#[derive(Serialize)]
#[serde(untagged)]
pub enum QEParamValue {
    Bool(bool),
    String(String),
    Int(i64),
}

/// Evaluate the given phabricator version number against the QE universe.
///
/// The API internally converts to the right unit ID based on the universe.
#[cfg(all(fbcode_build, target_os = "linux"))]
pub async fn evaluate_qe(
    phabricator_version_number: u64,
    universe: &str,
    param: &str,
    expect: QEParamValue,
    step: supertd_events::Step,
) -> bool {
    use sandcastle_qe2_client::QE2;
    use tracing::info;

    let value_for_logging: serde_json::Value;
    let qe = QE2::from_unit_id(phabricator_version_number, &[universe]).await;
    let ret = match &expect {
        QEParamValue::Bool(expect) => {
            let qe_value = qe.get_bool(universe, param, false);
            value_for_logging = qe_value.into();
            qe_value == *expect
        }
        QEParamValue::String(expect) => {
            let qe_value = qe.get_string(universe, param, "");
            value_for_logging = qe_value.clone().into();
            qe_value == *expect
        }
        QEParamValue::Int(expect) => {
            let qe_value = qe.get_int(universe, param, 0);
            value_for_logging = qe_value.into();
            qe_value == *expect
        }
    };

    let expect_str = serde_json::to_string(&expect).unwrap_or_else(|_| "<unknown>".into());
    info!(
        "Check {param} from QE {universe}, value {value_for_logging} (expected {expect_str}): {ret}"
    );
    crate::scuba!(event: QE_CHECK, data: json!({
        "phabricator_version_number": phabricator_version_number,
        "param": param,
        "universe": universe,
        "value": value_for_logging,
        "expect": expect,
        "result": ret,
        "step": step,
        "in_experiment": qe.in_experiment(universe),
    }));
    ret
}

#[cfg(not(all(fbcode_build, target_os = "linux")))]
pub async fn evaluate_qe(
    _phabricator_version_number: u64,
    _universe: &str,
    _param: &str,
    _expect: QEParamValue,
    _step: supertd_events::Step,
) -> bool {
    false
}
