use super::{CheckKind, DoctorCheck, Severity};

pub(super) fn check(admission: plakous_hosted::session_store::StorageAdmission) -> DoctorCheck {
    use plakous_hosted::session_store::StorageAdmission;
    let (severity, result, detail) = match admission {
        StorageAdmission::Available => (
            Severity::Ok,
            "available",
            crate::storage_space::healthy_notice(),
        ),
        StorageAdmission::Warning(notice) => (Severity::Warning, "warning", notice),
        StorageAdmission::Refused(notice) => (Severity::Error, "new work refused", notice),
    };
    DoctorCheck::new(
        CheckKind::HomeFreeSpace,
        "storage",
        severity,
        result,
        detail,
        (severity != Severity::Ok).then_some(
            "Free space on the product home volume, then retry. No state is deleted automatically.",
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_hosted::session_store::StorageAdmission;

    #[test]
    fn doctor_distinguishes_warning_and_admission_refusal() {
        let warning = check(StorageAdmission::Warning(
            "test volume: 1024 MiB free".into(),
        ));
        assert_eq!(warning.kind, CheckKind::HomeFreeSpace);
        assert_eq!(warning.severity, Severity::Warning);
        assert!(warning.detail.contains("1024 MiB"));
        assert!(warning.remediation.is_some());
        let refused = check(StorageAdmission::Refused("test volume: 0 MiB free".into()));
        assert_eq!(refused.severity, Severity::Error);
        let ready = check(StorageAdmission::Available);
        assert_eq!(ready.severity, Severity::Ok);
        assert!(ready.remediation.is_none());
    }
}
