//! Exercise the gateway over its real Unix listener from product unit tests.

#![allow(clippy::unwrap_used, reason = "isolated offline gateway fixture")]

use std::path::Path;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use plakous::gateway::protocol::admin::{AdminRequest, AdminResponse};

use super::{AdminLink, GatewayAddress, HostCustody, ServerConfig};
use crate::home::paths::PlakPaths;

pub(crate) struct GatewayFixture {
    socket: tempfile::TempDir,
    served: Option<JoinHandle<Result<(), String>>>,
}

impl GatewayFixture {
    pub(crate) fn start(paths: &PlakPaths, state: &Path) -> Self {
        paths.ensure_layout().unwrap();
        let base = if cfg!(target_os = "macos") {
            std::path::PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let socket = tempfile::tempdir_in(base).unwrap();
        let listener = socket.path().join("g");
        std::fs::create_dir_all(state).unwrap();
        let config = ServerConfig::parse(
            paths,
            &format!(
                "socket = {:?}\nstate_root = {:?}\n",
                listener.to_str().unwrap(),
                state.to_str().unwrap(),
            ),
        )
        .unwrap();
        let app = crate::commands::application::PlakousApplication::new(
            state.to_path_buf(),
            None,
            None,
            HostCustody::Local,
        )
        .unwrap()
        .with_account_home(Some(paths.clone()));
        let instance = plakous::gateway::server::new_instance().unwrap();
        let bridge = super::ops::OpsBridge::new(app, paths.clone(), instance);
        let gateway = super::service::Gateway::open_with(paths, config, Box::new(bridge)).unwrap();
        let served = std::thread::spawn(move || gateway.serve_foreground());
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::os::unix::net::UnixStream::connect(&listener).is_err() {
            assert!(!served.is_finished(), "gateway exited before accepting");
            assert!(Instant::now() < deadline, "gateway did not accept");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self {
            socket,
            served: Some(served),
        }
    }

    pub(crate) fn custody(&self) -> HostCustody {
        HostCustody::from_admin(AdminLink::local_service(self.address())).unwrap()
    }

    pub(super) fn address(&self) -> GatewayAddress {
        GatewayAddress::Unix(self.socket.path().join("g"))
    }
}

impl Drop for GatewayFixture {
    fn drop(&mut self) {
        let response = super::client::admin(&self.address(), None, &AdminRequest::Drain {});
        if matches!(response, Ok(AdminResponse::Draining {}))
            && let Some(served) = self.served.take()
        {
            let drained = served.join();
            if !std::thread::panicking() {
                drained.unwrap().unwrap();
            }
        } else if !std::thread::panicking() {
            panic!("could not drain fixture gateway: {response:?}");
        }
    }
}
