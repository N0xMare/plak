use super::*;

#[test]
fn unknown_accounts_refuse_selection_and_resume_before_credentials() {
    use plakous_hosted::capabilities::AccountAdmissionError;
    let config = crate::product_config::ProductConfig::default_product();
    for account in ["openai/work/extra", "missing", "", "xai", "OPENAI"] {
        let failure =
            crate::CliFailure::from(plakous_hosted::capabilities::require(account).unwrap_err());
        assert_eq!(failure.exit_code(), 2);
        assert!(!failure.is_capability_unavailable());
        assert_eq!(
            std::error::Error::source(&failure)
                .unwrap()
                .downcast_ref::<AccountAdmissionError>(),
            Some(&AccountAdmissionError::UnknownAccount {
                account: account.into()
            })
        );
        let mut record = crate::store::sessions::SessionRecord::new("/unused".into());
        record.account = Some(account.into());
        let resumed = admit_session(&record).unwrap_err();
        assert_eq!(resumed.message(), failure.message());
    }
    let record = crate::store::sessions::SessionRecord::new("/unused".into());
    admit_session(&record).unwrap();
}
