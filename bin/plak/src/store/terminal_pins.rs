//! Write-ahead workspace pins, never shell recovery or process identity authority.

use super::ProductStore;
use plakous::runner::protocol::operator::{OperatorNamespace, TerminalKey};

pub(crate) struct TerminalPinGuard(std::fs::File);

impl TerminalPinGuard {
    pub(crate) fn take(paths: &crate::home::paths::PlakPaths) -> Result<Self, String> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let path = paths
            .product_database()
            .with_file_name("operator-terminals.lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
            .open(path)
            .map_err(|_| "terminal pin lock unavailable")?;
        file.try_lock().map_err(|_| "terminal pin admission busy")?;
        Ok(Self(file))
    }
}

impl Drop for TerminalPinGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

pub(crate) struct StoredTerminalFence(pub crate::home::paths::PlakPaths);

impl plakous_hosted::workspace_reclamation::ReclamationFence for StoredTerminalFence {
    fn admit(
        &self,
        runner: &str,
        path: &std::path::Path,
    ) -> Result<
        Box<dyn plakous_hosted::workspace_reclamation::ReclamationAdmission>,
        plakous_hosted::workspace_reclamation::VcsError,
    > {
        let refused = || {
            plakous_hosted::workspace_reclamation::VcsError::mint_unavailable(
                "operator terminal workspace authority unavailable",
            )
        };
        let guard = TerminalPinGuard::take(&self.0).map_err(|_| refused())?;
        let store = ProductStore::open(&self.0).map_err(|_| refused())?;
        let pins = store.terminal_pins().map_err(|_| refused())?;
        let path = physical(runner, path).map_err(|_| refused())?;
        for pin in pins.into_iter().filter(|pin| pin.runner == runner) {
            let workspace = physical(runner, &pin.workspace).map_err(|_| refused())?;
            if workspace.starts_with(&path) || path.starts_with(&workspace) {
                return Err(refused());
            }
        }
        Ok(Box::new(guard))
    }
}

pub(crate) fn physical(runner: &str, path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err("invalid workspace spelling".into());
    }
    if runner != "local" {
        return Ok(path.to_owned());
    }
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                for name in missing.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.file_name().ok_or("workspace has no name")?);
                ancestor = ancestor.parent().ok_or("workspace has no parent")?;
            }
            Err(_) => return Err("workspace identity unavailable".into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalPin {
    pub host_generation: u64,
    pub host_id: String,
    pub generation: String,
    pub namespace: OperatorNamespace,
    pub runner: String,
    pub instance: String,
    pub key: TerminalKey,
    pub workspace: std::path::PathBuf,
    pub digest: String,
}

impl TerminalPin {
    pub(crate) fn identity(&self) -> Result<String, String> {
        serde_json::to_string(&(
            &self.host_id,
            &self.generation,
            &self.namespace,
            &self.runner,
            &self.key,
        ))
        .map_err(|_| "invalid terminal pin identity".into())
    }
}

impl ProductStore {
    pub(crate) fn terminal_session_pinned(&self, session: &str) -> Result<bool, String> {
        self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM operator_terminal_pins WHERE json_extract(record,'$.key.session')=?1)",
            [session], |row| row.get(0),
        ).map_err(|_| "terminal session pins unavailable".into())
    }
    pub(crate) fn terminal_pins(&self) -> Result<Vec<TerminalPin>, String> {
        let mut statement = self
            .connection
            .prepare("SELECT CASE WHEN length(identity)<=4096 THEN identity END, CASE WHEN length(record)<=65536 THEN record END FROM operator_terminal_pins ORDER BY identity LIMIT 1025")
            .map_err(|_| "terminal pins unavailable")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|_| "terminal pins unavailable")?;
        let mut pins = Vec::new();
        for row in rows {
            let (identity, row) = row.map_err(|_| "terminal pin unreadable")?;
            let pin: TerminalPin =
                serde_json::from_str(&row).map_err(|_| "invalid terminal pin")?;
            if pin.identity()? != identity || !pin.workspace.is_absolute() {
                return Err("invalid terminal pin identity".into());
            }
            pins.push(pin);
        }
        if pins.len() > 1024 {
            return Err("terminal pin table exceeds its bound".into());
        }
        Ok(pins)
    }

    pub(crate) fn reserve_terminal(&mut self, pin: &TerminalPin) -> Result<(), String> {
        self.durability(true)?;
        let identity = pin.identity()?;
        let record = serde_json::to_string(pin).map_err(|_| "invalid terminal pin")?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| "terminal pin write unavailable")?;
        let count: i64 = transaction
            .query_row("SELECT count(*) FROM operator_terminal_pins", [], |row| {
                row.get(0)
            })
            .map_err(|_| "terminal pin count unavailable")?;
        if count >= 1024 {
            return Err("terminal pin quota exhausted".into());
        }
        transaction
            .execute(
                "INSERT INTO operator_terminal_pins(identity,record) VALUES(?1,?2)",
                (identity, record),
            )
            .map_err(|_| "terminal identity already reserved")?;
        transaction
            .commit()
            .map_err(|_| "terminal pin commit unavailable".into())
    }

    pub(crate) fn release_terminal(&mut self, pin: &TerminalPin) -> Result<(), String> {
        self.durability(true)?;
        let changed = self
            .connection
            .execute(
                "DELETE FROM operator_terminal_pins WHERE identity=?1 AND record=?2",
                (
                    pin.identity()?,
                    serde_json::to_string(pin).map_err(|_| "invalid terminal pin")?,
                ),
            )
            .map_err(|_| "terminal pin release unavailable")?;
        if changed != 1 {
            return Err("terminal pin changed before release".into());
        }
        Ok(())
    }
}
