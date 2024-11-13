/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

pub enum QEParamValue {
    Bool,
    String(String),
    Int(i64),
}

#[cfg(all(fbcode_build, target_os = "linux"))]
pub fn evaluate_qe(unit_id: u64, universe: &str, param: &str, expect: QEParamValue) -> bool {
    use sandcastle_qe2_client::QE2;
    use tracing::info;

    let qe = crate::executor::run_as_sync(QE2::from_unit_id(unit_id, &[universe]));
    let ret = match expect {
        QEParamValue::Bool => qe.get_bool(universe, param, false),
        QEParamValue::String(expect) => {
            let qe_value = qe.get_string(universe, param, "");
            qe_value == expect
        }
        QEParamValue::Int(expect) => {
            let qe_value = qe.get_int(universe, param, 0);
            qe_value == expect
        }
    };

    info!("Check {param} from QE {universe}: {ret}");
    crate::scuba!(event: VERSE_QE_CHECK, data: json!({ "param": param, "universe": universe, "result": ret }));
    ret
}

#[cfg(not(all(fbcode_build, target_os = "linux")))]
pub fn evaluate_qe(unit_id: u64, universe: &str, param: &str, expect: QEParamValue) -> bool {
    return false;
}
