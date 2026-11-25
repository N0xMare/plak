//! `<home>/gateway/callers.json`. The gateway is the only writer. A caller id
//! is never reused, and the file stores a hash of the secret, never the key.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use plakous::gateway::protocol::TextError;
use plakous::gateway::protocol::admin::CallerRow;
use plakous::gateway::protocol::scope::{CallerId, CallerKey, ScopeSet, SourceNet};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use super::io_file::write_private;

#[derive(Clone, Debug)]
pub(crate) struct CallerRecord {
    pub caller: CallerId,
    pub scopes: ScopeSet,
    pub sources: Vec<SourceNet>,
    pub issued_at_unix_ms: u64,
    secret_sha256: [u8; 32],
    pub revoked_at_unix_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct File {
    version: u32,
    callers: Vec<Stored>,
}

#[derive(Clone, Debug, serde::Serialize, Deserialize)]
struct Stored {
    caller: String,
    scopes: String,
    sources: Vec<String>,
    issued_at_unix_ms: u64,
    secret_sha256: String,
    revoked_at_unix_ms: Option<u64>,
}

#[derive(Debug)]
pub(crate) enum CallerError {
    Exists,
    Unknown,
    Io(String),
    Malformed(String),
}

pub(crate) struct Callers {
    path: std::path::PathBuf,
    records: Vec<CallerRecord>,
}

impl Callers {
    pub(crate) fn load(path: &Path) -> Result<Self, CallerError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(CallerError::Io(error.to_string())),
        };
        let records = if text.trim().is_empty() {
            Vec::new()
        } else {
            let file: File = serde_json::from_str(&text)
                .map_err(|error| CallerError::Malformed(error.to_string()))?;
            if file.version != 1 {
                return Err(CallerError::Malformed(
                    "callers.json version is not 1".to_owned(),
                ));
            }
            file.callers
                .into_iter()
                .map(decode_stored)
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(Self {
            path: path.to_path_buf(),
            records,
        })
    }

    pub(crate) fn add(
        &mut self,
        caller: CallerId,
        scopes: ScopeSet,
        sources: Vec<SourceNet>,
    ) -> Result<CallerKey, CallerError> {
        if sources.is_empty() {
            return Err(CallerError::Malformed(
                "a caller key needs at least one source network".to_owned(),
            ));
        }
        if self.records.iter().any(|record| record.caller == caller) {
            return Err(CallerError::Exists);
        }
        let secret = random_secret()?;
        let key = CallerKey::from_secret(caller.clone(), *secret);
        let mut next = self.records.clone();
        next.push(CallerRecord {
            caller,
            scopes,
            sources,
            issued_at_unix_ms: now_ms(),
            secret_sha256: hash_secret(key.secret()),
            revoked_at_unix_ms: None,
        });
        store_records(&self.path, &next)?;
        self.records = next;
        Ok(key)
    }

    pub(crate) fn rotate(&mut self, caller: &CallerId) -> Result<CallerKey, CallerError> {
        let index = self.live_index(caller)?;
        let secret = random_secret()?;
        let key = CallerKey::from_secret(caller.clone(), *secret);
        let mut next = self.records.clone();
        next[index].secret_sha256 = hash_secret(key.secret());
        next[index].issued_at_unix_ms = now_ms();
        store_records(&self.path, &next)?;
        self.records = next;
        Ok(key)
    }

    pub(crate) fn remove(&mut self, caller: &CallerId) -> Result<(), CallerError> {
        let index = self.live_index(caller)?;
        let mut next = self.records.clone();
        next[index].revoked_at_unix_ms = Some(now_ms());
        store_records(&self.path, &next)?;
        self.records = next;
        Ok(())
    }

    /// Authenticates a key. A miss and a revoked key are the same answer.
    pub(crate) fn authenticate(&self, key: &CallerKey) -> Option<&CallerRecord> {
        let digest = hash_secret(key.secret());
        self.records.iter().find(|record| {
            record.caller == *key.id()
                && record.revoked_at_unix_ms.is_none()
                && constant_eq(&record.secret_sha256, &digest)
        })
    }

    pub(crate) fn rows(&self) -> Vec<CallerRow> {
        self.records
            .iter()
            .map(|record| CallerRow {
                caller: record.caller.clone(),
                scopes: record.scopes.clone(),
                sources: record.sources.clone(),
                issued_at_unix_ms: record.issued_at_unix_ms,
                revoked_at_unix_ms: record.revoked_at_unix_ms,
            })
            .collect()
    }

    fn live_index(&self, caller: &CallerId) -> Result<usize, CallerError> {
        self.records
            .iter()
            .position(|record| record.caller == *caller && record.revoked_at_unix_ms.is_none())
            .ok_or(CallerError::Unknown)
    }
}

fn store_records(path: &Path, records: &[CallerRecord]) -> Result<(), CallerError> {
    let file = StoredFile {
        version: 1,
        callers: records.iter().map(encode_stored).collect(),
    };
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|error| CallerError::Malformed(error.to_string()))?;
    write_private(path, &bytes).map_err(|error| CallerError::Io(error.to_string()))
}

#[derive(serde::Serialize)]
struct StoredFile {
    version: u32,
    callers: Vec<Stored>,
}

fn encode_stored(record: &CallerRecord) -> Stored {
    Stored {
        caller: record.caller.as_str().to_owned(),
        scopes: record.scopes.to_string(),
        sources: record.sources.iter().map(ToString::to_string).collect(),
        issued_at_unix_ms: record.issued_at_unix_ms,
        secret_sha256: hex_encode(&record.secret_sha256),
        revoked_at_unix_ms: record.revoked_at_unix_ms,
    }
}

fn decode_stored(stored: Stored) -> Result<CallerRecord, CallerError> {
    let malformed = |error: TextError| CallerError::Malformed(error.to_string());
    let secret_sha256 = hex_decode(&stored.secret_sha256)
        .ok_or_else(|| CallerError::Malformed("caller secret hash is malformed".to_owned()))?;
    Ok(CallerRecord {
        caller: CallerId::new(stored.caller).map_err(malformed)?,
        scopes: ScopeSet::parse(&stored.scopes).map_err(malformed)?,
        sources: stored
            .sources
            .iter()
            .map(|source| SourceNet::parse(source).map_err(malformed))
            .collect::<Result<Vec<_>, _>>()?,
        issued_at_unix_ms: stored.issued_at_unix_ms,
        secret_sha256,
        revoked_at_unix_ms: stored.revoked_at_unix_ms,
    })
}

fn hash_secret(secret: &[u8; 32]) -> [u8; 32] {
    let digest = Sha256::digest(secret);
    let mut out = [0_u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn constant_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    use subtle::ConstantTimeEq as _;
    bool::from(left.ct_eq(right))
}

fn random_secret() -> Result<zeroize::Zeroizing<[u8; 32]>, CallerError> {
    let mut bytes = zeroize::Zeroizing::new([0_u8; 32]);
    getrandom::fill(&mut *bytes).map_err(|error| CallerError::Io(error.to_string()))?;
    Ok(bytes)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

fn hex_decode(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0_u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        let high = hex_nibble(text.as_bytes()[index * 2])?;
        let low = hex_nibble(text.as_bytes()[index * 2 + 1])?;
        *byte = (high << 4) | low;
    }
    Some(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
