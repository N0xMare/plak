use super::{CheckKind, CompositionHostStatus, DoctorCheck, Requirement, Severity};
use std::path::Path;

pub(super) fn composition_checks(
    requirement: Requirement,
    host: CompositionHostStatus,
    python_containment_required: bool,
    #[cfg_attr(
        not(all(target_os = "linux", target_arch = "x86_64")),
        allow(
            unused_variables,
            reason = "XDG_RUNTIME_DIR is a Linux containment probe only"
        )
    )]
    runtime_directory: Option<&Path>,
) -> Vec<DoctorCheck> {
    let missing_severity = match requirement {
        Requirement::Optional => Severity::Warning,
        Requirement::Required => Severity::Error,
    };
    let mut checks = Vec::new();
    checks.push(match host {
        CompositionHostStatus::Included { identity } => DoctorCheck::new(
            CheckKind::CompositionHost,
            "composition",
            Severity::Ok,
            "included",
            identity,
            None,
        ),
        CompositionHostStatus::Unavailable(error) => DoctorCheck::new(
            CheckKind::CompositionHost,
            "composition",
            missing_severity,
            "unavailable",
            error,
            Some(if python_containment_required {
                composition_host_unavailable_remediation()
            } else {
                javascript_host_unavailable_remediation()
            }),
        ),
    });
    let _ = (python_containment_required, runtime_directory);
    checks
}

fn composition_host_unavailable_remediation() -> &'static str {
    "set PLAK_PY_HOST to the plakous-py-host worker or place plakous-py-host beside plak"
}

fn javascript_host_unavailable_remediation() -> &'static str {
    "set PLAK_JS_HOST to plakous-js-host or place plakous-js-host beside plak"
}
