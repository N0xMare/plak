//! Test builds simulate distinct runner releases by executable identity.

use std::ffi::OsString;
use std::path::PathBuf;

use super::hook_environment;

const CANDIDATE: &str = "PLAK_TEST_HOST_UPGRADE_RUNNER_IMAGE";
const REVISION: &str = "PLAK_TEST_HOST_UPGRADE_RUNNER_REVISION";
const PROTOCOL: &str = "PLAK_TEST_HOST_UPGRADE_RUNNER_PROTOCOL";
const NEXT: &str = "PLAK_TEST_HOST_UPGRADE_RUNNER_NEXT_IMAGE";
const NEXT_REVISION: &str = "PLAK_TEST_HOST_UPGRADE_RUNNER_NEXT_REVISION";

pub(crate) fn upgrade_runner_revision() -> Option<String> {
    [(CANDIDATE, REVISION), (NEXT, NEXT_REVISION)]
        .into_iter()
        .filter(|(image, _)| matches_image(image))
        .find_map(|(_, revision)| hook_environment(revision)?.into_string().ok())
}

pub(crate) fn upgrade_runner_protocol() -> Option<u32> {
    if !matches_image(CANDIDATE) {
        return None;
    }
    hook_environment(PROTOCOL)?.to_str()?.parse().ok()
}

fn matches_image(name: &str) -> bool {
    hook_environment(name)
        .and_then(|image| PathBuf::from(image).canonicalize().ok())
        .is_some_and(|image| {
            std::env::current_exe()
                .and_then(|path| path.canonicalize())
                .is_ok_and(|path| path == image)
        })
}

pub(crate) fn upgrade_runner_environment() -> Vec<(&'static str, OsString)> {
    [CANDIDATE, REVISION, PROTOCOL, NEXT, NEXT_REVISION]
        .into_iter()
        .filter_map(|name| hook_environment(name).map(|value| (name, value)))
        .collect()
}
