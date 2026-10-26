pub(crate) fn process_started() {}
pub(crate) fn writer_registered() {}
pub(crate) fn quit_requested() {}
pub(crate) fn exiting() {}
pub(crate) fn remaining_exit_budget() -> std::time::Duration {
    std::time::Duration::from_secs(1)
}
