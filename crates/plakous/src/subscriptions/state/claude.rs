use super::{
    NativeSubscriptionError, SubscriptionPaths, SubscriptionState, acquire_private_filesystem_lock,
    new_random_identity, read_private_bounded, write_private_atomic,
};

impl SubscriptionPaths {
    pub(in crate::subscriptions) fn claude_client_device_id(
        &self,
    ) -> Result<plakous_anthropic::ClaudeClientDeviceId, NativeSubscriptionError> {
        self.prepare()?;
        let path = self.layout_root().join("client-device-id");
        let lock =
            acquire_private_filesystem_lock(&self.layout_root().join(".client-device-id.lock"))?;
        lock.verify()?;
        let value = match read_private_bounded(&path) {
            Ok(bytes) => {
                String::from_utf8(bytes).map_err(|_| NativeSubscriptionError::UnsafeStorage)?
            }
            Err(NativeSubscriptionError::AcceptanceRequired) => {
                let value = new_random_identity()?;
                write_private_atomic(&path, value.as_bytes())?;
                value
            }
            Err(error) => return Err(error),
        };
        lock.verify()?;
        plakous_anthropic::ClaudeClientDeviceId::new(value)
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)
    }
}
impl SubscriptionState {
    pub(in crate::subscriptions) fn claude_client_device_id(
        &self,
    ) -> Result<plakous_anthropic::ClaudeClientDeviceId, NativeSubscriptionError> {
        self.paths.claude_client_device_id()
    }
}
