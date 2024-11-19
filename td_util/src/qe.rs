/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use serde::Serialize;

#[derive(Serialize)]
#[serde(untagged)]
pub enum QEParamValue {
    Bool(bool),
    String(String),
    Int(i64),
}

#[cfg(all(fbcode_build, target_os = "linux"))]
pub fn evaluate_qe(unit_id: u64, universe: &str, param: &str, expect: QEParamValue) -> bool {
    use sandcastle_qe2_client::QE2;
    use tracing::info;

    let value_for_logging: serde_json::Value;
    let qe = crate::executor::run_as_sync(QE2::from_unit_id(unit_id, &[universe]));
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
    crate::scuba!(event: VERSE_QE_CHECK, data: json!({
        "param": param,
        "universe": universe,
        "value": value_for_logging,
        "expect": expect,
        "result": ret,
    }));
    ret
}

#[cfg(not(all(fbcode_build, target_os = "linux")))]
pub fn evaluate_qe(unit_id: u64, universe: &str, param: &str, expect: QEParamValue) -> bool {
    return false;
}
