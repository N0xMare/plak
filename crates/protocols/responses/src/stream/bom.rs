//! Leading UTF-8 BOM stripping for dialects with [`BomPolicy::Strip`].

use crate::dialect::{BomPolicy, UTF8_BOM};

/// Mutable lead buffer used only while a dialect may still strip a BOM.
///
/// `None` means BOM handling is finished or disabled. `Some(bytes)` holds the
/// incomplete prefix of the stream while deciding whether a BOM is present.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BomLead {
    inner: Option<Vec<u8>>,
}

impl BomLead {
    /// Creates a lead buffer for the dialect BOM policy.
    #[must_use]
    pub(crate) const fn for_policy(policy: BomPolicy) -> Self {
        match policy {
            BomPolicy::Strip => Self {
                inner: Some(Vec::new()),
            },
            BomPolicy::Ignore => Self { inner: None },
        }
    }

    /// Returns whether unfinished BOM-prefix bytes remain at EOF.
    #[must_use]
    pub(crate) fn has_pending_bytes(&self) -> bool {
        self.inner.as_ref().is_some_and(|lead| !lead.is_empty())
    }

    /// Strips an optional UTF-8 BOM from the first stream bytes.
    ///
    /// Returns `None` while the lead is still an incomplete BOM prefix and the
    /// decoder must wait for more input before framing begins.
    pub(crate) fn take_bytes_after_bom(&mut self, chunk: &[u8]) -> Option<Vec<u8>> {
        let Some(lead) = self.inner.as_mut() else {
            return Some(chunk.to_vec());
        };
        lead.extend_from_slice(chunk);
        if lead.is_empty() {
            return None;
        }
        let compared = lead.len().min(UTF8_BOM.len());
        if lead[..compared] == UTF8_BOM[..compared] {
            if lead.len() < UTF8_BOM.len() {
                return None;
            }
            lead.drain(..UTF8_BOM.len());
        }
        let rest = std::mem::take(lead);
        self.inner = None;
        Some(rest)
    }
}
