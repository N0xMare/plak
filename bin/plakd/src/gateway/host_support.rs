use super::HostCustody;

pub(crate) use super::holder::{HolderLock, acquire_shared_creating};

/// A gateway host refuses provider key variables: the gateway's own
/// environment is the one that resolves keys.
pub(crate) fn host_may_open_store(custody: &HostCustody) -> Result<(), String> {
    if custody.address().is_some() {
        let set = crate::environment::host::provider_key_env_set();
        if !set.is_empty() {
            return Err(format!(
                "a host with [gateway] address set refuses provider key environment variables: {}",
                set.join(", ")
            ));
        }
    }
    Ok(())
}
