use std::fmt;

use bytes::Bytes;
use http::HeaderValue;
use http::header::InvalidHeaderValue;
use zeroize::Zeroize;

/// Request bytes backed by an allocation that is zeroized on its final drop.
///
/// Cloning this value, converting it with [`Self::into_bytes`], and cloning the
/// resulting [`Bytes`] all retain the same backing owner. The owner zeroizes
/// its complete vector capacity when the last shared clone is dropped.
///
/// This guarantee applies only to this allocation. A source copied before
/// construction or a later conversion that performs a deep copy has separate
/// ownership and is not covered.
#[derive(Clone)]
pub struct SensitiveBytes(Bytes);

impl SensitiveBytes {
    /// Takes ownership of request bytes without copying them.
    #[must_use]
    pub fn new(value: Vec<u8>) -> Self {
        Self::from_owner(ZeroizingOwner::new(value))
    }

    /// Returns the number of sensitive bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether there are no sensitive bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Converts this value to the ordinary request-body [`Bytes`] API without
    /// copying or losing the zeroizing backing owner.
    #[must_use]
    pub fn into_bytes(self) -> Bytes {
        self.0
    }

    fn from_owner(owner: ZeroizingOwner) -> Self {
        Self(Bytes::from_owner(owner))
    }

    #[cfg(test)]
    fn observed(value: Vec<u8>, drop_probe: std::sync::Arc<DropProbe>) -> Self {
        Self::from_owner(ZeroizingOwner::observed(value, drop_probe))
    }
}

impl From<Vec<u8>> for SensitiveBytes {
    fn from(value: Vec<u8>) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for SensitiveBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SensitiveBytes")
            .field("bytes", &self.len())
            .finish_non_exhaustive()
    }
}

/// Constructs a validated, redaction-marked header value whose shared backing
/// allocation is zeroized after its final [`HeaderValue`] or [`Bytes`] clone is
/// dropped.
///
/// The input is first converted to an exact [`Bytes`] value so
/// [`HeaderValue::from_maybe_shared`] preserves its zeroizing owner. Invalid
/// header bytes return an error and drop that owner immediately.
///
/// # Errors
///
/// Returns [`InvalidHeaderValue`] when `value` contains bytes forbidden in an
/// HTTP header value.
pub fn sensitive_header_value(
    value: impl Into<SensitiveBytes>,
) -> Result<HeaderValue, InvalidHeaderValue> {
    let bytes = value.into().into_bytes();
    let mut value = HeaderValue::from_maybe_shared(bytes)?;
    value.set_sensitive(true);
    Ok(value)
}

struct ZeroizingOwner {
    value: Vec<u8>,
    #[cfg(test)]
    drop_probe: Option<std::sync::Arc<DropProbe>>,
}

impl ZeroizingOwner {
    fn new(value: Vec<u8>) -> Self {
        Self {
            value,
            #[cfg(test)]
            drop_probe: None,
        }
    }

    #[cfg(test)]
    fn observed(value: Vec<u8>, drop_probe: std::sync::Arc<DropProbe>) -> Self {
        Self {
            value,
            drop_probe: Some(drop_probe),
        }
    }
}

impl AsRef<[u8]> for ZeroizingOwner {
    fn as_ref(&self) -> &[u8] {
        &self.value
    }
}

impl Drop for ZeroizingOwner {
    fn drop(&mut self) {
        self.value.zeroize();
        #[cfg(test)]
        if let Some(drop_probe) = &self.drop_probe {
            drop_probe.record_zeroized_drop(self.value.is_empty());
        }
    }
}

#[cfg(test)]
#[derive(Default)]
struct DropProbe {
    drops: std::sync::atomic::AtomicUsize,
    zeroized_before_drop: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl DropProbe {
    fn record_zeroized_drop(&self, zeroized: bool) {
        self.zeroized_before_drop
            .store(zeroized, std::sync::atomic::Ordering::Release);
        self.drops.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn drops(&self) -> usize {
        self.drops.load(std::sync::atomic::Ordering::Acquire)
    }

    fn zeroized_before_drop(&self) -> bool {
        self.zeroized_before_drop
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
