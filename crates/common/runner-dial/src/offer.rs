use crate::{Error, PairKey};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use secrecy::{ExposeSecret, SecretBox};
use std::fmt;
use zeroize::Zeroizing;

pub(crate) fn validate_url(value: &str) -> Result<(), Error> {
    let url = url::Url::parse(value).map_err(|_| Error::Invalid)?;
    let secure = url.scheme() == "wss";
    #[cfg(feature = "test-hooks")]
    let secure = secure || url.scheme() == "ws";
    // Inspect the raw authority too: URL normalization drops an empty userinfo.
    let authority = value
        .split_once("://")
        .ok_or(Error::Invalid)?
        .1
        .split(['/', '?', '#'])
        .next()
        .ok_or(Error::Invalid)?;
    if !secure
        || url.host_str().is_none()
        || authority.contains('@')
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
        || value.contains('\\')
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

/// Operator-carried single-use pairing material. Never log or put it in argv.
pub struct Offer {
    url: String,
    host_id: String,
    host_key: [u8; 32],
    id: [u8; 16],
    secret: SecretBox<[u8; 16]>,
    runner_name: String,
    expiry: u64,
}
impl fmt::Debug for Offer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Offer([REDACTED])")
    }
}
impl Offer {
    /// Creates a fresh offer id and secret. Expiry is displayed, never enforced here.
    /// # Errors
    /// Refuses an invalid URL, empty identity/name or unavailable CSPRNG.
    pub fn generate(
        url: String,
        host_id: String,
        host_key: [u8; 32],
        runner_name: String,
        expiry: u64,
    ) -> Result<Self, Error> {
        validate_url(&url)?;
        if host_id.is_empty() || runner_name.is_empty() {
            return Err(Error::Invalid);
        }
        let mut offer_id = [0; 16];
        let mut secret = Zeroizing::new([0; 16]);
        getrandom::fill(&mut offer_id).map_err(Error::Entropy)?;
        getrandom::fill(&mut *secret).map_err(Error::Entropy)?;
        Ok(Self {
            url,
            host_id,
            host_key,
            id: offer_id,
            secret: SecretBox::new(Box::new(*secret)),
            runner_name,
            expiry,
        })
    }
    /// Parses the seven strict unpadded base64url fields of `plak-dial:1`.
    /// # Errors
    /// Refuses malformed encoding, field sizes, URL, identity/name or expiry.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let encoded = text.strip_prefix("plak-dial:1:").ok_or(Error::Invalid)?;
        let fields: Vec<_> = encoded.split('.').collect();
        if fields.len() != 7 {
            return Err(Error::Invalid);
        }
        let decode = |index: usize| {
            URL_SAFE_NO_PAD
                .decode(fields[index])
                .map(Zeroizing::new)
                .map_err(|_| Error::Invalid)
        };
        let string = |index: usize| -> Result<String, Error> {
            let bytes = decode(index)?;
            std::str::from_utf8(&bytes)
                .map(str::to_owned)
                .map_err(|_| Error::Invalid)
        };
        let url = string(0)?;
        validate_url(&url)?;
        let host_id = string(1)?;
        let host_key = decode(2)?
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invalid)?;
        let offer_id = decode(3)?
            .as_slice()
            .try_into()
            .map_err(|_| Error::Invalid)?;
        let secret = decode(4)?;
        let secret: [u8; 16] = secret.as_slice().try_into().map_err(|_| Error::Invalid)?;
        let secret = SecretBox::new(Box::new(secret));
        let runner_name = string(5)?;
        let expiry = string(6)?;
        if host_id.is_empty()
            || runner_name.is_empty()
            || expiry.is_empty()
            || !expiry.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(Error::Invalid);
        }
        let expiry = expiry.parse().map_err(|_| Error::Invalid)?;
        Ok(Self {
            url,
            host_id,
            host_key,
            id: offer_id,
            secret,
            runner_name,
            expiry,
        })
    }
    /// Explicitly exposes the offer for a private output channel, never diagnostics.
    #[must_use]
    pub fn expose_encoded(&self) -> secrecy::SecretString {
        let expiry = self.expiry.to_string();
        let fields: [&[u8]; 7] = [
            self.url.as_bytes(),
            self.host_id.as_bytes(),
            &self.host_key,
            &self.id,
            self.secret.expose_secret(),
            self.runner_name.as_bytes(),
            expiry.as_bytes(),
        ];
        let mut out = String::from("plak-dial:1:");
        for (index, field) in fields.iter().enumerate() {
            if index > 0 {
                out.push('.');
            }
            let encoded = Zeroizing::new(URL_SAFE_NO_PAD.encode(field));
            out.push_str(&encoded);
        }
        secrecy::SecretString::from(out)
    }
    /// Validated endpoint, retaining the exact spelling bound into the prologue.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }
    /// Claimed host identity; the product must check its own identity and allowlist.
    #[must_use]
    pub fn host_id(&self) -> &str {
        &self.host_id
    }
    /// Pinned host public key. Show its fingerprint, not its raw bytes.
    #[must_use]
    pub fn host_key(&self) -> &[u8; 32] {
        &self.host_key
    }
    /// Non-secret offer lookup identifier.
    #[must_use]
    pub fn offer_id(&self) -> [u8; 16] {
        self.id
    }
    /// Intended runner name, for product validation and pending confirmation.
    #[must_use]
    pub fn runner_name(&self) -> &str {
        &self.runner_name
    }
    /// Unix seconds; the host, not this parser, enforces expiration.
    #[must_use]
    pub fn expiry(&self) -> u64 {
        self.expiry
    }
    /// Derives the stored MAC key without exposing the offer secret.
    #[must_use]
    pub fn pair_key(&self) -> PairKey {
        PairKey::derive(self.secret.expose_secret())
    }
}
