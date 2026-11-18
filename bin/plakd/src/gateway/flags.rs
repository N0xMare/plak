//! `<home>/gateway/flags.json`: the kill switch. The gateway is its only writer.

use std::path::Path;

use plakous::gateway::protocol::GatewayAccount;
use plakous::gateway::protocol::scope::CallerId;

use super::io_file::write_private;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct File {
    version: u32,
    disabled_all: bool,
    disabled_accounts: Vec<String>,
    disabled_callers: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Flags {
    path: std::path::PathBuf,
    disabled_all: bool,
    accounts: Vec<GatewayAccount>,
    callers: Vec<CallerId>,
}

impl Flags {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.to_string()),
        };
        if text.trim().is_empty() {
            return Ok(Self {
                path: path.to_path_buf(),
                ..Self::default()
            });
        }
        let file: File = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if file.version != 1 {
            return Err("flags.json version is not 1".to_owned());
        }
        Ok(Self {
            path: path.to_path_buf(),
            disabled_all: file.disabled_all,
            accounts: file
                .disabled_accounts
                .into_iter()
                .map(GatewayAccount::new)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?,
            callers: file
                .disabled_callers
                .into_iter()
                .map(CallerId::new)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?,
        })
    }

    pub(crate) fn disabled_all(&self) -> bool {
        self.disabled_all
    }

    pub(crate) fn account_disabled(&self, account: &GatewayAccount) -> bool {
        self.accounts.iter().any(|disabled| disabled == account)
    }

    pub(crate) fn caller_disabled(&self, caller: &CallerId) -> bool {
        self.callers.iter().any(|disabled| disabled == caller)
    }

    pub(crate) fn disable_all(&mut self) -> Result<(), String> {
        self.disabled_all = true;
        self.store()
    }

    pub(crate) fn disable_account(&mut self, account: GatewayAccount) -> Result<(), String> {
        if !self.accounts.contains(&account) {
            self.accounts.push(account);
        }
        self.store()
    }

    pub(crate) fn disable_caller(&mut self, caller: CallerId) -> Result<(), String> {
        if !self.callers.contains(&caller) {
            self.callers.push(caller);
        }
        self.store()
    }

    pub(crate) fn enable_all(&mut self) -> Result<(), String> {
        self.disabled_all = false;
        self.store()
    }

    pub(crate) fn enable_account(&mut self, account: &GatewayAccount) -> Result<(), String> {
        self.accounts.retain(|disabled| disabled != account);
        self.store()
    }

    pub(crate) fn enable_caller(&mut self, caller: &CallerId) -> Result<(), String> {
        self.callers.retain(|disabled| disabled != caller);
        self.store()
    }

    pub(crate) fn summary(&self) -> String {
        format!(
            "all={} accounts={} callers={}",
            self.disabled_all,
            self.accounts.len(),
            self.callers.len()
        )
    }

    fn store(&self) -> Result<(), String> {
        let file = File {
            version: 1,
            disabled_all: self.disabled_all,
            disabled_accounts: self.accounts.iter().map(ToString::to_string).collect(),
            disabled_callers: self
                .callers
                .iter()
                .map(|id| id.as_str().to_owned())
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(|error| error.to_string())?;
        write_private(&self.path, &bytes).map_err(|error| error.to_string())
    }
}
