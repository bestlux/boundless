use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const INSTALL_DOCTOR_SCHEMA_VERSION: u32 = 1;
pub const REQUIRED_PAYLOADS: [&str; 5] = [
    "boundlessctl.exe",
    "boundlesstray.exe",
    "boundlessd.exe",
    "boundless-service.exe",
    "boundless-input-injector.exe",
];
pub const VERSIONED_EXECUTABLES: [&str; 4] = [
    "boundlessctl",
    "boundlesstray",
    "boundlessd",
    "boundless-service",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstallEvidence {
    pub platform_supported: bool,
    pub collection_errors: Vec<String>,
    pub product_codes: Vec<String>,
    pub display_version: String,
    pub install_root: String,
    pub manifest_present: bool,
    pub manifest_version: String,
    pub manifest_executables: BTreeMap<String, String>,
    pub payloads_present: BTreeMap<String, bool>,
    pub service_account: String,
    pub service_binary_path: String,
    pub service_binary_path_matches: bool,
    pub service_running: bool,
    pub daemon_api_healthy: bool,
    pub daemon_running: bool,
    pub daemon_runtime_version: String,
    pub executable_versions: BTreeMap<String, String>,
    pub tray_count: usize,
    pub tray_path_matches: bool,
    pub tray_responding: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallCheck {
    pub id: String,
    pub ok: bool,
    pub expected: String,
    pub actual: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallDoctorReport {
    pub schema_version: u32,
    pub command: String,
    pub ok: bool,
    pub checks: Vec<InstallCheck>,
    pub evidence: InstallEvidence,
}

pub fn evaluate_install_evidence(evidence: InstallEvidence) -> InstallDoctorReport {
    let mut checks = Vec::new();
    push_check(
        &mut checks,
        "platform.supported",
        evidence.platform_supported,
        "windows",
        if evidence.platform_supported {
            "windows"
        } else {
            "unsupported"
        },
    );
    let collection_actual = if evidence.collection_errors.is_empty() {
        "none".to_string()
    } else {
        evidence.collection_errors.join("; ")
    };
    push_check(
        &mut checks,
        "collection.windows",
        evidence.collection_errors.is_empty(),
        "no collection errors",
        &collection_actual,
    );
    push_check(
        &mut checks,
        "product.registration",
        evidence.product_codes.len() == 1,
        "exactly one related MSI product",
        &format!("{} product(s)", evidence.product_codes.len()),
    );
    push_check(
        &mut checks,
        "product.display_version",
        manifest_version_matches_display(&evidence.manifest_version, &evidence.display_version),
        &evidence.manifest_version,
        &evidence.display_version,
    );
    push_check(
        &mut checks,
        "payload.manifest",
        evidence.manifest_present && !evidence.manifest_version.is_empty(),
        "present with version",
        if evidence.manifest_present {
            &evidence.manifest_version
        } else {
            "missing"
        },
    );

    for payload in REQUIRED_PAYLOADS {
        let present = evidence
            .payloads_present
            .get(payload)
            .copied()
            .unwrap_or(false);
        let mapped = evidence
            .manifest_executables
            .values()
            .any(|value| value.eq_ignore_ascii_case(payload));
        push_check(
            &mut checks,
            &format!("payload.{payload}"),
            present && mapped,
            "present and declared in manifest",
            if present && mapped {
                "present"
            } else {
                "missing or undeclared"
            },
        );
    }

    push_check(
        &mut checks,
        "service.account",
        evidence.service_account.eq_ignore_ascii_case("LocalSystem"),
        "LocalSystem",
        &evidence.service_account,
    );
    push_check(
        &mut checks,
        "service.binary_path",
        evidence.service_binary_path_matches,
        "installed Program Files binary",
        &evidence.service_binary_path,
    );
    push_check(
        &mut checks,
        "service.running",
        evidence.service_running,
        "Running",
        if evidence.service_running {
            "Running"
        } else {
            "not Running"
        },
    );
    push_check(
        &mut checks,
        "daemon.api",
        evidence.daemon_api_healthy && evidence.daemon_running,
        "healthy and running",
        if evidence.daemon_api_healthy && evidence.daemon_running {
            "healthy"
        } else {
            "unhealthy"
        },
    );
    push_check(
        &mut checks,
        "daemon.version",
        evidence.daemon_runtime_version == evidence.manifest_version
            && !evidence.manifest_version.is_empty(),
        &evidence.manifest_version,
        &evidence.daemon_runtime_version,
    );

    for executable in VERSIONED_EXECUTABLES {
        let actual = evidence
            .executable_versions
            .get(executable)
            .cloned()
            .unwrap_or_default();
        push_check(
            &mut checks,
            &format!("executable.version.{executable}"),
            !actual.is_empty() && actual == evidence.manifest_version,
            &evidence.manifest_version,
            if actual.is_empty() {
                "unavailable"
            } else {
                &actual
            },
        );
    }

    push_check(
        &mut checks,
        "tray.count",
        evidence.tray_count == 1,
        "1",
        &evidence.tray_count.to_string(),
    );
    push_check(
        &mut checks,
        "tray.path",
        evidence.tray_path_matches,
        "installed tray path",
        if evidence.tray_path_matches {
            "matched"
        } else {
            "mismatch"
        },
    );
    push_check(
        &mut checks,
        "tray.responsive",
        evidence.tray_responding,
        "responsive",
        if evidence.tray_responding {
            "responsive"
        } else {
            "unresponsive"
        },
    );

    let ok = checks.iter().all(|check| check.ok);
    InstallDoctorReport {
        schema_version: INSTALL_DOCTOR_SCHEMA_VERSION,
        command: "doctor.install".to_string(),
        ok,
        checks,
        evidence,
    }
}

fn push_check(checks: &mut Vec<InstallCheck>, id: &str, ok: bool, expected: &str, actual: &str) {
    checks.push(InstallCheck {
        id: id.to_string(),
        ok,
        expected: expected.to_string(),
        actual: actual.to_string(),
        message: if ok {
            "passed".to_string()
        } else {
            format!("expected {expected}; observed {actual}")
        },
    });
}

pub fn manifest_version_matches_display(manifest: &str, display: &str) -> bool {
    !manifest.is_empty()
        && !display.is_empty()
        && (manifest == display
            || manifest
                .strip_prefix(display)
                .is_some_and(|suffix| suffix.starts_with('-') || suffix.starts_with('+')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_evidence() -> InstallEvidence {
        let manifest_executables = BTreeMap::from([
            ("cli".to_string(), "boundlessctl.exe".to_string()),
            ("tray".to_string(), "boundlesstray.exe".to_string()),
            ("daemon".to_string(), "boundlessd.exe".to_string()),
            ("service".to_string(), "boundless-service.exe".to_string()),
            (
                "input_injector".to_string(),
                "boundless-input-injector.exe".to_string(),
            ),
        ]);
        InstallEvidence {
            platform_supported: true,
            collection_errors: Vec::new(),
            product_codes: vec!["{PRODUCT}".to_string()],
            display_version: "5.0.16".to_string(),
            install_root: r"C:\Program Files\Boundless".to_string(),
            manifest_present: true,
            manifest_version: "5.0.16".to_string(),
            payloads_present: REQUIRED_PAYLOADS
                .into_iter()
                .map(|name| (name.to_string(), true))
                .collect(),
            manifest_executables,
            service_account: "LocalSystem".to_string(),
            service_binary_path: r"C:\Program Files\Boundless\boundless-service.exe".to_string(),
            service_binary_path_matches: true,
            service_running: true,
            daemon_api_healthy: true,
            daemon_running: true,
            daemon_runtime_version: "5.0.16".to_string(),
            executable_versions: VERSIONED_EXECUTABLES
                .into_iter()
                .map(|name| (name.to_string(), "5.0.16".to_string()))
                .collect(),
            tray_count: 1,
            tray_path_matches: true,
            tray_responding: true,
        }
    }

    #[test]
    fn healthy_evidence_passes_every_check() {
        let report = evaluate_install_evidence(valid_evidence());
        assert!(report.ok);
        assert!(report.checks.iter().all(|check| check.ok));
    }

    #[test]
    fn failures_are_aggregated_in_one_report() {
        let mut evidence = valid_evidence();
        evidence.product_codes.clear();
        evidence.service_running = false;
        evidence.daemon_runtime_version = "5.0.15".to_string();
        evidence.tray_count = 2;
        let report = evaluate_install_evidence(evidence);
        let failures = report
            .checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| check.id.as_str())
            .collect::<Vec<_>>();
        assert!(failures.contains(&"product.registration"));
        assert!(failures.contains(&"service.running"));
        assert!(failures.contains(&"daemon.version"));
        assert!(failures.contains(&"tray.count"));
    }

    fn assert_check_fails(evidence: InstallEvidence, check_id: &str) {
        let report = evaluate_install_evidence(evidence);
        let check = report
            .checks
            .iter()
            .find(|check| check.id == check_id)
            .unwrap_or_else(|| panic!("missing check {check_id}"));
        assert!(!report.ok, "report unexpectedly passed for {check_id}");
        assert!(!check.ok, "check unexpectedly passed: {check_id}");
    }

    #[test]
    fn every_install_postcondition_has_a_failure_case() {
        type EvidenceMutation = Box<dyn Fn(&mut InstallEvidence)>;
        let cases: Vec<(&str, EvidenceMutation)> = vec![
            (
                "platform.supported",
                Box::new(|value| value.platform_supported = false),
            ),
            (
                "collection.windows",
                Box::new(|value| value.collection_errors.push("probe failed".to_string())),
            ),
            (
                "product.registration",
                Box::new(|value| value.product_codes.clear()),
            ),
            (
                "product.display_version",
                Box::new(|value| value.display_version = "5.0.15".to_string()),
            ),
            (
                "payload.manifest",
                Box::new(|value| value.manifest_present = false),
            ),
            (
                "service.account",
                Box::new(|value| value.service_account = "LocalService".to_string()),
            ),
            (
                "service.binary_path",
                Box::new(|value| value.service_binary_path_matches = false),
            ),
            (
                "service.running",
                Box::new(|value| value.service_running = false),
            ),
            (
                "daemon.api",
                Box::new(|value| value.daemon_api_healthy = false),
            ),
            (
                "daemon.version",
                Box::new(|value| value.daemon_runtime_version = "5.0.15".to_string()),
            ),
            ("tray.count", Box::new(|value| value.tray_count = 2)),
            (
                "tray.path",
                Box::new(|value| value.tray_path_matches = false),
            ),
            (
                "tray.responsive",
                Box::new(|value| value.tray_responding = false),
            ),
        ];

        for (check_id, mutate) in cases {
            let mut evidence = valid_evidence();
            mutate(&mut evidence);
            assert_check_fails(evidence, check_id);
        }

        for payload in REQUIRED_PAYLOADS {
            let mut evidence = valid_evidence();
            evidence.payloads_present.insert(payload.to_string(), false);
            assert_check_fails(evidence, &format!("payload.{payload}"));
        }

        for executable in VERSIONED_EXECUTABLES {
            let mut evidence = valid_evidence();
            evidence
                .executable_versions
                .insert(executable.to_string(), "5.0.15".to_string());
            assert_check_fails(evidence, &format!("executable.version.{executable}"));
        }
    }

    #[test]
    fn display_version_accepts_dogfood_and_build_metadata() {
        assert!(manifest_version_matches_display(
            "5.0.16-dogfood.1",
            "5.0.16"
        ));
        assert!(manifest_version_matches_display("5.0.16+meta", "5.0.16"));
        assert!(!manifest_version_matches_display("5.0.160", "5.0.16"));
    }
}
