//! Caller keys and scopes.
//!
//! A caller key is `plakgw1.<id>.<secret>`: a caller id and 32 random bytes in
//! unpadded base64url. The gateway stores only a hash of the secret, the
//! key's scopes and the source networks it may arrive from. A scope list is
//! comma-separated `<capability>:<accounts>`, such as `relay:*,admin:codex/*`,
//! where `<accounts>` is `*`, `<kind>/*` (the kind's default and named
//! accounts) or one exact account. A key holds `relay` unless `admin` is
//! granted explicitly; a same-UID peer on the gateway's own socket is the
//! local administrator and needs no key.

use std::fmt;
use std::net::IpAddr;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use zeroize::Zeroizing;

use crate::account::GatewayAccount;
use crate::text::{TextError, is_segment};

/// The caller key prefix and format version.
pub const CALLER_KEY_PREFIX: &str = "plakgw1";
/// The most scopes one key holds.
pub const MAX_SCOPES: usize = 16;

/// A caller id: `[a-z][a-z0-9-]{0,31}`. Ids are never reused.
#[derive(
    Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct CallerId(String);

impl CallerId {
    /// Checks the grammar.
    ///
    /// # Errors
    /// Refuses anything but `[a-z][a-z0-9-]{0,31}`.
    pub fn new(value: impl Into<String>) -> Result<Self, TextError> {
        let value = value.into();
        if is_segment(&value) {
            Ok(Self(value))
        } else {
            Err(TextError::new("caller id is malformed"))
        }
    }

    /// The id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CallerId {
    type Error = TextError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<CallerId> for String {
    fn from(value: CallerId) -> Self {
        value.0
    }
}

/// One caller key. Its debug output names only the caller id.
pub struct CallerKey {
    id: CallerId,
    secret: Zeroizing<[u8; 32]>,
}

impl CallerKey {
    /// Builds a key from freshly drawn random bytes.
    #[must_use]
    pub fn from_secret(id: CallerId, secret: [u8; 32]) -> Self {
        Self {
            id,
            secret: Zeroizing::new(secret),
        }
    }

    /// Parses `plakgw1.<id>.<secret>`.
    ///
    /// # Errors
    /// Refuses another prefix, a malformed id, and a secret that is not the
    /// canonical unpadded base64url encoding of 32 bytes.
    pub fn parse(text: &str) -> Result<Self, TextError> {
        let malformed = || TextError::new("caller key is malformed");
        let rest = text
            .strip_prefix(CALLER_KEY_PREFIX)
            .and_then(|rest| rest.strip_prefix('.'))
            .ok_or_else(malformed)?;
        let (id, encoded) = rest.split_once('.').ok_or_else(malformed)?;
        let id = CallerId::new(id).map_err(|_| malformed())?;
        let decoded = Zeroizing::new(URL_SAFE_NO_PAD.decode(encoded).map_err(|_| malformed())?);
        let mut secret = Zeroizing::new([0_u8; 32]);
        if decoded.len() != secret.len() {
            return Err(malformed());
        }
        secret.copy_from_slice(&decoded);
        Ok(Self { id, secret })
    }

    /// The caller id.
    #[must_use]
    pub const fn id(&self) -> &CallerId {
        &self.id
    }

    /// The secret bytes the gateway hashes and compares.
    #[must_use]
    pub fn secret(&self) -> &[u8; 32] {
        &self.secret
    }

    /// The key text, for the caller's key header or key file.
    #[must_use]
    pub fn expose(&self) -> Zeroizing<String> {
        let encoded = Zeroizing::new(URL_SAFE_NO_PAD.encode(self.secret.as_slice()));
        Zeroizing::new(format!(
            "{CALLER_KEY_PREFIX}.{}.{}",
            self.id.as_str(),
            encoded.as_str()
        ))
    }
}

impl fmt::Debug for CallerKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CallerKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// What a scope permits.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Capability {
    /// Facts, refresh-on-demand, relay, account listing, status and
    /// allowance reads.
    Relay,
    /// Login, logout, keys, removal, forced refresh and remote disable.
    Admin,
}

impl Capability {
    const fn name(self) -> &'static str {
        match self {
            Self::Relay => "relay",
            Self::Admin => "admin",
        }
    }
}

/// Which accounts a scope covers.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum AccountGlob {
    /// Every account.
    Any,
    /// A kind's default account and every named account of that kind.
    Kind(String),
    /// Exactly one account.
    Exact(GatewayAccount),
}

impl AccountGlob {
    /// Whether `account` is covered.
    #[must_use]
    pub fn matches(&self, account: &GatewayAccount) -> bool {
        match self {
            Self::Any => true,
            Self::Kind(kind) => account.kind() == kind,
            Self::Exact(exact) => exact == account,
        }
    }
}

/// One scope.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Scope {
    /// What it permits.
    pub capability: Capability,
    /// Which accounts.
    pub accounts: AccountGlob,
}

impl fmt::Display for Scope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let capability = self.capability.name();
        match &self.accounts {
            AccountGlob::Any => write!(formatter, "{capability}:*"),
            AccountGlob::Kind(kind) => write!(formatter, "{capability}:{kind}/*"),
            AccountGlob::Exact(account) => write!(formatter, "{capability}:{account}"),
        }
    }
}

/// A caller key's scopes, kept in canonical sorted order.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ScopeSet(Vec<Scope>);

impl ScopeSet {
    /// Parses a comma-separated scope list.
    ///
    /// # Errors
    /// Refuses an empty list, more than [`MAX_SCOPES`] scopes, an unknown
    /// capability, a malformed account pattern and a repeated scope.
    pub fn parse(text: &str) -> Result<Self, TextError> {
        let mut scopes = text
            .split(',')
            .map(parse_scope)
            .collect::<Result<Vec<_>, _>>()?;
        if scopes.len() > MAX_SCOPES {
            return Err(TextError::new("scope list is too long"));
        }
        scopes.sort();
        if scopes.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(TextError::new("scope list repeats a scope"));
        }
        Ok(Self(scopes))
    }

    /// Whether any scope permits `capability` on `account`.
    #[must_use]
    pub fn allows(&self, capability: Capability, account: &GatewayAccount) -> bool {
        self.0
            .iter()
            .any(|scope| scope.capability == capability && scope.accounts.matches(account))
    }

    /// The scopes in canonical order.
    #[must_use]
    pub fn scopes(&self) -> &[Scope] {
        &self.0
    }
}

fn parse_scope(text: &str) -> Result<Scope, TextError> {
    let (capability, accounts) = text
        .split_once(':')
        .ok_or_else(|| TextError::new("scope is not capability:accounts"))?;
    let capability = match capability {
        "relay" => Capability::Relay,
        "admin" => Capability::Admin,
        _ => return Err(TextError::new("scope capability is unknown")),
    };
    let accounts = match accounts {
        "*" => AccountGlob::Any,
        _ => match accounts.strip_suffix("/*") {
            Some(kind) if is_segment(kind) => AccountGlob::Kind(kind.to_owned()),
            Some(_) => return Err(TextError::new("scope account kind is malformed")),
            None => AccountGlob::Exact(GatewayAccount::new(accounts)?),
        },
    };
    Ok(Scope {
        capability,
        accounts,
    })
}

impl fmt::Display for ScopeSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, scope) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str(",")?;
            }
            write!(formatter, "{scope}")?;
        }
        Ok(())
    }
}

impl TryFrom<String> for ScopeSet {
    type Error = TextError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<ScopeSet> for String {
    fn from(value: ScopeSet) -> Self {
        value.to_string()
    }
}

/// A source network a caller key may arrive from: one address or a CIDR
/// prefix, such as `100.101.102.103` or `100.64.0.0/10`.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct SourceNet {
    address: IpAddr,
    prefix: u8,
}

impl SourceNet {
    /// Parses an address or `address/prefix` whose host bits are zero.
    ///
    /// # Errors
    /// Refuses a malformed address, a prefix beyond the address width and a
    /// prefix with host bits set.
    pub fn parse(text: &str) -> Result<Self, TextError> {
        let malformed = || TextError::new("source network is malformed");
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let address: IpAddr = address.parse().map_err(|_| malformed())?;
        let width = if address.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => width,
            Some(prefix) if prefix.starts_with('0') && prefix != "0" => return Err(malformed()),
            Some(prefix) => prefix.parse::<u8>().map_err(|_| malformed())?,
        };
        if prefix > width {
            return Err(malformed());
        }
        let net = Self { address, prefix };
        if net.masked(address) == address {
            Ok(net)
        } else {
            Err(malformed())
        }
    }

    /// Whether `peer` is inside this network. An IPv4 network never matches
    /// an IPv6 peer, including a mapped one.
    #[must_use]
    pub fn contains(&self, peer: IpAddr) -> bool {
        self.address.is_ipv4() == peer.is_ipv4() && self.masked(peer) == self.address
    }

    fn masked(&self, address: IpAddr) -> IpAddr {
        match address {
            IpAddr::V4(v4) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                IpAddr::V4((u32::from(v4) & mask).into())
            }
            IpAddr::V6(v6) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                IpAddr::V6((u128::from(v6) & mask).into())
            }
        }
    }
}

impl fmt::Display for SourceNet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let width = if self.address.is_ipv4() { 32 } else { 128 };
        if self.prefix == width {
            write!(formatter, "{}", self.address)
        } else {
            write!(formatter, "{}/{}", self.address, self.prefix)
        }
    }
}

impl TryFrom<String> for SourceNet {
    type Error = TextError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<SourceNet> for String {
    fn from(value: SourceNet) -> Self {
        value.to_string()
    }
}
