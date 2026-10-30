//! Credential authority, auth slots and header-binding validation for one attempt.

use super::{
    AuthReservationMask, CredentialGenerationFingerprint, Digest, HeaderName, LedgerPolicy,
    ResponsesAttemptBuildError,
};

/// Closed authorization-slot requirement for one Responses dialect route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesAuthSlots {
    /// One primary platform/single-slot authorization.
    Primary,
    /// One secondary subscription authorization (for example native Codex).
    Secondary,
    /// Both independent authorizations are required for the same generation.
    PrimaryAndSecondary,
}

impl ResponsesAuthSlots {
    pub(super) const fn mask(self) -> AuthReservationMask {
        match self {
            Self::Primary => AuthReservationMask::PRIMARY,
            Self::Secondary => AuthReservationMask::SECONDARY,
            Self::PrimaryAndSecondary => {
                AuthReservationMask::PRIMARY.union(AuthReservationMask::SECONDARY)
            }
        }
    }

    pub(super) const fn identity_label(self) -> &'static [u8] {
        match self {
            Self::Primary => b"primary",
            Self::Secondary => b"secondary",
            Self::PrimaryAndSecondary => b"primary+secondary",
        }
    }
}

/// One credential authorization slot used by a secret header binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesAuthSlot {
    /// Primary platform/single-slot credential.
    Primary,
    /// Secondary subscription credential.
    Secondary,
}

impl ResponsesAuthSlot {
    pub(super) const fn identity_label(self) -> &'static [u8] {
        match self {
            Self::Primary => b"primary",
            Self::Secondary => b"secondary",
        }
    }
}

/// One account-authority slot used by an account-secret header binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesAccountSlot {
    /// Primary account authority.
    Primary,
    /// Secondary/subscription account authority.
    Secondary,
}

/// Exact non-secret authority for all credential and account-secret slots in
/// one immutable physical request.
///
/// Credential bytes never enter this value. Each present authorization slot
/// carries only its owning authority's generation fingerprint and, when the
/// route emits an account-secret header, its non-secret account authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesCredentialAuthority {
    primary: Option<ResponsesSlotAuthority>,
    secondary: Option<ResponsesSlotAuthority>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ResponsesSlotAuthority {
    generation: CredentialGenerationFingerprint,
    account: Option<Digest>,
}

impl ResponsesCredentialAuthority {
    /// Builds an exact single-primary-slot authority.
    #[must_use]
    pub const fn primary(generation: CredentialGenerationFingerprint) -> Self {
        Self {
            primary: Some(ResponsesSlotAuthority {
                generation,
                account: None,
            }),
            secondary: None,
        }
    }

    /// Builds an exact single-secondary-slot authority.
    #[must_use]
    pub const fn secondary(generation: CredentialGenerationFingerprint) -> Self {
        Self {
            primary: None,
            secondary: Some(ResponsesSlotAuthority {
                generation,
                account: None,
            }),
        }
    }

    /// Builds exact independent primary and secondary authorities.
    #[must_use]
    pub const fn primary_and_secondary(
        primary: CredentialGenerationFingerprint,
        secondary: CredentialGenerationFingerprint,
    ) -> Self {
        Self {
            primary: Some(ResponsesSlotAuthority {
                generation: primary,
                account: None,
            }),
            secondary: Some(ResponsesSlotAuthority {
                generation: secondary,
                account: None,
            }),
        }
    }

    /// Adds the exact non-secret authority for one required account-secret
    /// header slot.
    ///
    /// # Errors
    ///
    /// Rejects a missing credential slot, a duplicate account authority, or an
    /// invalid authority digest.
    pub fn with_account(
        mut self,
        slot: ResponsesAccountSlot,
        authority: Digest,
    ) -> Result<Self, ResponsesAttemptBuildError> {
        if authority.version() == 0 {
            return Err(ResponsesAttemptBuildError::Binding);
        }
        let target = match slot {
            ResponsesAccountSlot::Primary => self.primary.as_mut(),
            ResponsesAccountSlot::Secondary => self.secondary.as_mut(),
        }
        .ok_or(ResponsesAttemptBuildError::AuthSlots)?;
        if target.account.replace(authority).is_some() {
            return Err(ResponsesAttemptBuildError::AuthSlots);
        }
        Ok(self)
    }

    /// Returns the exact authorization slots represented by this authority.
    #[must_use]
    pub const fn auth_slots(&self) -> ResponsesAuthSlots {
        match (&self.primary, &self.secondary) {
            (Some(_), None) => ResponsesAuthSlots::Primary,
            (None, Some(_)) => ResponsesAuthSlots::Secondary,
            (Some(_), Some(_)) => ResponsesAuthSlots::PrimaryAndSecondary,
            (None, None) => unreachable!(),
        }
    }

    /// Returns the exact generation fingerprint for a named slot.
    #[must_use]
    pub const fn generation(
        &self,
        slot: ResponsesAuthSlot,
    ) -> Option<CredentialGenerationFingerprint> {
        match slot {
            ResponsesAuthSlot::Primary => match &self.primary {
                Some(authority) => Some(authority.generation),
                None => None,
            },
            ResponsesAuthSlot::Secondary => match &self.secondary {
                Some(authority) => Some(authority.generation),
                None => None,
            },
        }
    }

    pub(super) const fn account(&self, slot: ResponsesAccountSlot) -> Option<Digest> {
        let authority = match slot {
            ResponsesAccountSlot::Primary => &self.primary,
            ResponsesAccountSlot::Secondary => &self.secondary,
        };
        match authority {
            Some(authority) => authority.account,
            None => None,
        }
    }
}

impl ResponsesAccountSlot {
    pub(super) const fn identity_label(self) -> &'static [u8] {
        match self {
            Self::Primary => b"primary-account",
            Self::Secondary => b"secondary-account",
        }
    }
}

/// Complete closed semantic classification for one immutable request header.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponsesHeaderBinding {
    /// A non-secret wire value pinned exactly by the dialect.
    PublicExact {
        /// Exact header name.
        name: HeaderName,
        /// Exact canonical public bytes expected on the request.
        value: Vec<u8>,
    },
    /// A non-secret exact routing marker whose presence is authorized by one
    /// credential slot. This covers provider headers whose names look secret
    /// (for example, contain `token`) while their pinned value is public.
    CredentialMarkerExact {
        /// Exact header name.
        name: HeaderName,
        /// Exact canonical public bytes expected on the request.
        value: Vec<u8>,
        /// Credential slot that authorizes emitting the marker.
        slot: ResponsesAuthSlot,
    },
    /// A credential-bearing value bound without hashing credential bytes.
    CredentialSecret {
        /// Exact header name.
        name: HeaderName,
        /// Authorization slot whose generation fingerprint binds the value.
        slot: ResponsesAuthSlot,
    },
    /// An account-bearing value bound by a distinct non-secret authority digest.
    AccountSecret {
        /// Exact header name.
        name: HeaderName,
        /// Account authority slot.
        slot: ResponsesAccountSlot,
        /// Non-secret exact account-authority fingerprint.
        authority: Digest,
    },
}

impl ResponsesHeaderBinding {
    pub(super) const fn name(&self) -> &HeaderName {
        match self {
            Self::PublicExact { name, .. }
            | Self::CredentialMarkerExact { name, .. }
            | Self::CredentialSecret { name, .. }
            | Self::AccountSecret { name, .. } => name,
        }
    }
}

pub(super) fn validate_auth_slots(
    ledger_policy: LedgerPolicy,
    auth_slots: ResponsesAuthSlots,
) -> Result<(), ResponsesAttemptBuildError> {
    if matches!(
        auth_slots,
        ResponsesAuthSlots::Secondary | ResponsesAuthSlots::PrimaryAndSecondary
    ) && !ledger_policy.dual_auth_slots
    {
        return Err(ResponsesAttemptBuildError::AuthSlots);
    }
    Ok(())
}

pub(super) fn validate_header_authority(
    header_bindings: &[ResponsesHeaderBinding],
    authority: &ResponsesCredentialAuthority,
) -> Result<(), ResponsesAttemptBuildError> {
    let mut primary_credential = 0_u8;
    let mut secondary_credential = 0_u8;
    let mut primary_account = 0_u8;
    let mut secondary_account = 0_u8;
    for binding in header_bindings {
        let count = match binding {
            ResponsesHeaderBinding::CredentialSecret {
                slot: ResponsesAuthSlot::Primary,
                ..
            } => Some(&mut primary_credential),
            ResponsesHeaderBinding::CredentialSecret {
                slot: ResponsesAuthSlot::Secondary,
                ..
            } => Some(&mut secondary_credential),
            ResponsesHeaderBinding::AccountSecret {
                slot: ResponsesAccountSlot::Primary,
                authority: binding_authority,
                ..
            } => {
                if authority.account(ResponsesAccountSlot::Primary) != Some(*binding_authority) {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                Some(&mut primary_account)
            }
            ResponsesHeaderBinding::AccountSecret {
                slot: ResponsesAccountSlot::Secondary,
                authority: binding_authority,
                ..
            } => {
                if authority.account(ResponsesAccountSlot::Secondary) != Some(*binding_authority) {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                Some(&mut secondary_account)
            }
            ResponsesHeaderBinding::PublicExact { .. }
            | ResponsesHeaderBinding::CredentialMarkerExact { .. } => None,
        };
        if let Some(count) = count {
            *count = (*count)
                .checked_add(1)
                .ok_or(ResponsesAttemptBuildError::AuthSlots)?;
        }
    }
    let exact = primary_credential
        == u8::from(authority.generation(ResponsesAuthSlot::Primary).is_some())
        && secondary_credential
            == u8::from(authority.generation(ResponsesAuthSlot::Secondary).is_some())
        && primary_account == u8::from(authority.account(ResponsesAccountSlot::Primary).is_some())
        && secondary_account
            == u8::from(authority.account(ResponsesAccountSlot::Secondary).is_some());
    if !exact {
        return Err(ResponsesAttemptBuildError::AuthSlots);
    }
    Ok(())
}

pub(super) fn header_name_requires_secret(name: &HeaderName) -> bool {
    let name = name.as_str();
    name.contains("authorization")
        || name.contains("api-key")
        || name.contains("apikey")
        || name.contains("token")
        || name.contains("cookie")
        || name.contains("account")
}
