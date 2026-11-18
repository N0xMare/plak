//! `<home>/gateway.toml`, the service file. It is separate from `config.toml`
//! so a host config edit never reaches the listener.

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

use super::address::{self, AddressError};
use super::io_file::write_private;
use crate::home::paths::{self, PlakPaths};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServerConfig {
    pub socket: PathBuf,
    pub listen_tcp: Option<SocketAddr>,
    pub state_root: PathBuf,
    pub key_dir: PathBuf,
    pub audit_retention_days: u16,
    pub max_inflight: u32,
    pub max_inflight_per_caller: u32,
    pub max_request_body_bytes: u64,
    pub allow_non_tailnet: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    socket: Option<String>,
    listen_tcp: Option<String>,
    state_root: Option<String>,
    key_dir: Option<String>,
    audit_retention_days: Option<u16>,
    max_inflight: Option<u32>,
    max_inflight_per_caller: Option<u32>,
    max_request_body_bytes: Option<u64>,
    allow_non_tailnet: Option<bool>,
}

impl ServerConfig {
    pub(crate) fn path(home: &PlakPaths) -> PathBuf {
        home.home().join("gateway.toml")
    }

    pub(crate) fn gateway_dir(home: &PlakPaths) -> PathBuf {
        home.home().join("gateway")
    }

    pub(crate) fn load(home: &PlakPaths) -> Result<Self, String> {
        let path = Self::path(home);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        };
        Self::parse(home, &text)
    }

    pub(crate) fn parse(home: &PlakPaths, text: &str) -> Result<Self, String> {
        let file: File = if text.trim().is_empty() {
            File {
                socket: None,
                listen_tcp: None,
                state_root: None,
                key_dir: None,
                audit_retention_days: None,
                max_inflight: None,
                max_inflight_per_caller: None,
                max_request_body_bytes: None,
                allow_non_tailnet: None,
            }
        } else {
            toml::from_str(text).map_err(|error| format!("gateway.toml: {error}"))?
        };
        let allow_non_tailnet = file.allow_non_tailnet.unwrap_or(false);
        let socket = match file.socket {
            Some(value) => absolute("socket", &value)?,
            None => default_socket(home),
        };
        let listen_tcp = match file.listen_tcp {
            Some(value) => {
                let addr: SocketAddr = value
                    .parse()
                    .map_err(|_| "listen_tcp must be <ip>:<port>".to_owned())?;
                address::admit_ip(addr.ip(), allow_non_tailnet).map_err(address_message)?;
                Some(addr)
            }
            None => None,
        };
        let state_root = match file.state_root {
            Some(value) => absolute("state_root", &value)?,
            None => paths::application_state_home()?,
        };
        let key_dir = match file.key_dir {
            Some(value) => absolute("key_dir", &value)?,
            None => home.secrets_dir(),
        };
        let audit_retention_days = file.audit_retention_days.unwrap_or(30);
        if !(1..=365).contains(&audit_retention_days) {
            return Err("audit_retention_days must be 1..=365".to_owned());
        }
        let max_inflight = file.max_inflight.unwrap_or(256);
        let max_inflight_per_caller = file.max_inflight_per_caller.unwrap_or(128);
        if max_inflight == 0 || max_inflight_per_caller == 0 {
            return Err("inflight limits must be at least 1".to_owned());
        }
        let max_request_body_bytes = file.max_request_body_bytes.unwrap_or(32 * 1024 * 1024);
        Ok(Self {
            socket,
            listen_tcp,
            state_root,
            key_dir,
            audit_retention_days,
            max_inflight,
            max_inflight_per_caller,
            max_request_body_bytes,
            allow_non_tailnet,
        })
    }

    /// Writes a fresh file. Refuses to replace one that already exists.
    pub(crate) fn init(
        home: &PlakPaths,
        listen_tcp: Option<SocketAddr>,
        key_dir: Option<PathBuf>,
    ) -> Result<Self, String> {
        let path = Self::path(home);
        if path.exists() {
            return Err(format!(
                "{} already exists; edit it, it was not replaced",
                path.display()
            ));
        }
        let mut config = Self::parse(home, "")?;
        if let Some(addr) = listen_tcp {
            address::admit_ip(addr.ip(), false).map_err(address_message)?;
            config.listen_tcp = Some(addr);
        }
        if let Some(dir) = key_dir {
            address::absolute_file(&dir).map_err(address_message)?;
            config.key_dir = dir;
        }
        let body = render(&config);
        write_private(&path, body.as_bytes()).map_err(|error| error.to_string())?;
        let _ = super::io_file::ensure_private_dir(&Self::gateway_dir(home));
        Ok(config)
    }
}

fn render(config: &ServerConfig) -> String {
    let mut body =
        String::from("# plakd gateway serve. Do not put provider secrets in this file.\n");
    body.push_str(&format!("socket = \"{}\"\n", config.socket.display()));
    if let Some(addr) = config.listen_tcp {
        body.push_str(&format!("listen_tcp = \"{addr}\"\n"));
    }
    body.push_str(&format!(
        "state_root = \"{}\"\nkey_dir = \"{}\"\naudit_retention_days = {}\nmax_inflight = {}\nmax_inflight_per_caller = {}\nmax_request_body_bytes = {}\nallow_non_tailnet = {}\n",
        config.state_root.display(),
        config.key_dir.display(),
        config.audit_retention_days,
        config.max_inflight,
        config.max_inflight_per_caller,
        config.max_request_body_bytes,
        config.allow_non_tailnet,
    ));
    body
}

fn default_socket(home: &PlakPaths) -> PathBuf {
    if let Some(runtime) = crate::environment::xdg_runtime_dir() {
        if runtime.is_absolute() {
            return runtime.join("plak/gateway.sock");
        }
    }
    ServerConfig::gateway_dir(home).join("gateway.sock")
}

fn absolute(name: &str, value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    address::absolute_file(&path).map_err(|error| format!("{name}: {error}"))?;
    Ok(path)
}

fn address_message(error: AddressError) -> String {
    error.0.to_owned()
}

pub(crate) fn callers_path(home: &PlakPaths) -> PathBuf {
    ServerConfig::gateway_dir(home).join("callers.json")
}

pub(crate) fn flags_path(home: &PlakPaths) -> PathBuf {
    ServerConfig::gateway_dir(home).join("flags.json")
}

pub(crate) fn audit_dir(home: &PlakPaths) -> PathBuf {
    ServerConfig::gateway_dir(home).join("audit")
}
