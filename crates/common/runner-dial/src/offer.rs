use crate::{Error, PairKey};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use secrecy::{ExposeSecret, ExposeSecretMut, SecretBox};
use std::fmt;
use zeroize::Zeroizing;

const MAX_OFFER_TEXT_BYTES: usize = 4096;
const MAX_RUNNER_NAME_BYTES: usize = 128;

fn validate_identity(host_id: &str, runner_name: &str) -> Result<(), Error> {
    let host = uuid::Uuid::parse_str(host_id).map_err(|_| Error::Invalid)?;
    if host.hyphenated().to_string() != host_id
        || runner_name.is_empty()
        || runner_name.len() > MAX_RUNNER_NAME_BYTES
        || !runner_name
            .bytes()
            .all(|byte| (b' '..=b'~').contains(&byte))
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

fn encoded_size(lengths: [usize; 7]) -> usize {
    "plak-dial:1:".len()
        + 6
        + lengths
            .into_iter()
            .map(|length| (length * 4).div_ceil(3))
            .sum::<usize>()
}

pub(crate) fn validate_url(value: &str) -> Result<(), Error> {
    let url = url::Url::parse(value).map_err(|_| Error::Invalid)?;
    let secure = url.scheme() == "wss";
    #[cfg(feature = "test-hooks")]
    let secure = secure || url.scheme() == "ws";
    if !secure
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !value.is_ascii()
        || url.as_str() != value
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
    /// Refuses invalid URL/identity/name, an offer over 4 KiB or unavailable CSPRNG.
    pub fn generate(
        url: String,
        host_id: String,
        host_key: [u8; 32],
        runner_name: String,
        expiry: u64,
    ) -> Result<Self, Error> {
        if url.len() > MAX_OFFER_TEXT_BYTES {
            return Err(Error::Invalid);
        }
        validate_url(&url)?;
        validate_identity(&host_id, &runner_name)?;
        if encoded_size([
            url.len(),
            host_id.len(),
            32,
            16,
            16,
            runner_name.len(),
            expiry.to_string().len(),
        ]) > MAX_OFFER_TEXT_BYTES
        {
            return Err(Error::Invalid);
        }
        let mut offer_id = [0; 16];
        let mut secret = SecretBox::<[u8; 16]>::default();
        getrandom::fill(&mut offer_id).map_err(Error::Entropy)?;
        getrandom::fill(secret.expose_secret_mut()).map_err(Error::Entropy)?;
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
    /// Parses the seven strict unpadded base64url fields of `plak-dial:1`.
    /// # Errors
    /// Refuses malformed encoding, field sizes, URL, identity/name or expiry.
    pub fn parse(text: &str) -> Result<Self, Error> {
        if text.len() > MAX_OFFER_TEXT_BYTES {
            return Err(Error::Invalid);
        }
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
        let mut secret = SecretBox::<[u8; 16]>::default();
        let length = URL_SAFE_NO_PAD
            .decode_slice(fields[4], secret.expose_secret_mut())
            .map_err(|_| Error::Invalid)?;
        if length != 16 {
            return Err(Error::Invalid);
        }
        let runner_name = string(5)?;
        validate_identity(&host_id, &runner_name)?;
        let expiry = string(6)?;
        if expiry.is_empty()
            || (expiry.len() > 1 && expiry.starts_with('0'))
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
        let mut out = String::with_capacity(encoded_size(fields.map(<[u8]>::len)));
        out.push_str("plak-dial:1:");
        for (index, field) in fields.iter().enumerate() {
            if index > 0 {
                out.push('.');
            }
            URL_SAFE_NO_PAD.encode_string(field, &mut out);
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
