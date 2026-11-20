use std::fs;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::CliResult;
use crate::deploy::fence::Fence;
use crate::deploy::inventory::{self, Platform};
use crate::deploy::launchd;
use crate::deploy::protocol::{
    Facts, GatewayHolder, GatewayIntent, GatewayObserved, GatewayOutcome, RunnerLink,
};
use crate::deploy::{target, timing};
use crate::shipping_artifact::{self as shipping, FileFacts, check, fail, io};
use crate::snapshot_install;

use super::{CUSTODY, Target, active_pid, capture, command, gateway_reply, text};

const NORMALIZE: &str = "gateway image change requires the generated dedicated plakd-gateway service; use the gateway service migration runbook before deploy";

#[derive(Debug, Eq, PartialEq)]
struct Service {
    command: PathBuf,
    link: RunnerLink,
    custody: String,
    state_root: PathBuf,
    state: State,
}

#[derive(Debug, Eq, PartialEq)]
enum State {
    Running(String),
    CleanExit,
    Transition,
}

impl Target<'_> {
    pub(in crate::deploy) fn gateway_observation(&self) -> CliResult<GatewayObserved> {
        let binary = self.process_image(&self.running_host_pid()?)?;
        let facts = self.facts(&binary)?;
        Ok(self.observe_gateway(&binary, &facts.gateway_spoken)?.0)
    }

    pub(super) fn observe_gateway(
        &self,
        binary: &Path,
        spoken: &[u32],
    ) -> CliResult<(GatewayObserved, Vec<String>)> {
        let service = self.gateway_service(binary)?;
        let State::Running(pid) = &service.state else {
            return Err(fail("credential gateway is not running"));
        };
        let process = self.process_image(pid)?;
        check(
            process == service.link.path,
            "gateway image change: loaded command does not resolve to the running image",
        )?;
        let custody = format!("via gateway unix:{}", self.host.gateway_socket.display());
        let status = self
            .product(binary, ["gateway", "status"], timing::PROBE)
            .map_err(|_| fail(CUSTODY))?;
        let status = text(&status)?;
        check(status.lines().any(|line| line == custody), CUSTODY)?;
        let instance = status
            .lines()
            .find_map(|line| line.strip_prefix("instance "))
            .filter(|instance| uuid::Uuid::parse_str(instance).is_ok())
            .ok_or_else(|| fail("gateway instance is unavailable"))?;
        // The serving host's own CLI checks the holder and negotiates its admin protocol.
        let reply = self.product(binary, ["gateway", "check"], timing::PROBE)?;
        let (versions, accounts) = gateway_reply(text(&reply)?, &custody, instance)?;
        let socket = fs::symlink_metadata(&self.host.gateway_socket).map_err(|_| fail(CUSTODY))?;
        check(
            socket.file_type().is_socket() && socket.uid() == self.host.node.uid,
            CUSTODY,
        )?;
        let socket_identity = (socket.dev(), socket.ino());
        let negotiated = negotiate(spoken, &versions);
        check(
            self.gateway_service(binary)? == service
                && self.process_image(pid)? == process
                && fs::symlink_metadata(&self.host.gateway_socket)
                    .is_ok_and(|now| (now.dev(), now.ino()) == socket_identity),
            "gateway changed during observation",
        )?;
        Ok((
            GatewayObserved {
                command: service.command,
                link: service.link,
                custody_sha256: service.custody,
                holder: GatewayHolder {
                    instance: instance.to_owned(),
                    pid: pid.clone(),
                    versions,
                    socket_identity,
                    negotiated,
                },
            },
            accounts,
        ))
    }

    pub(in crate::deploy) fn switch_gateway(
        &self,
        intent: &GatewayIntent,
        begin: bool,
    ) -> CliResult<GatewayOutcome> {
        let mut fence = Fence::lock(&self.host.node, "gateway", intent.ticket)?;
        let mut drain = Fence::lock(&self.host.node, "gateway-drain", intent.ticket)?;
        if begin {
            fence.active()?;
        }
        let binary = self.gateway_source_host(intent)?;
        let facts = self.facts(&binary)?;
        let destination = target::staged_in(&self.host.node, &intent.candidate)?;
        let new = RunnerLink {
            path: destination.clone(),
            sha256: intent.candidate.sha256.clone(),
        };
        check(
            intent.source.link.sha256 == self.host.gateway_sha256
                && new != intent.source.link
                && self.gateway_image(&intent.source.link.path)? == intent.source.link
                && negotiate(&facts.gateway_spoken, &intent.served)
                    .is_some_and(|v| v >= intent.minimum),
            "gateway source pin or incumbent protocol does not qualify",
        )?;
        let loaded = self.checked_gateway_service(&binary, intent, &new)?;
        if matches!(loaded.state, State::Running(_)) {
            let (observed, _) = self.observe_gateway(&binary, &facts.gateway_spoken)?;
            if observed.link == new {
                fence.active()?;
                check(
                    !drain.untouched()?,
                    "gateway changed without the recorded drain",
                )?;
                verify_successor(intent, &new, &observed)?;
                return Ok(GatewayOutcome::Ready { observed });
            }
            check(
                observed == intent.source,
                "gateway holder changed before drain",
            )?;
            if !fence.untouched()? || !begin && drain.untouched()? {
                if fence.untouched()? {
                    fence.cancel()?;
                }
                return Ok(GatewayOutcome::Withdrawn);
            }
            check(
                begin,
                "gateway drain outcome is unresolved; wait for the recorded holder to exit or inspect on target",
            )?;
            if !self.drain_gateway(intent, &mut drain, &destination, &loaded)? {
                fence.cancel()?;
                return Ok(GatewayOutcome::Withdrawn);
            }
        }
        fence.active()?;
        check(
            !drain.untouched()?,
            "gateway went inactive without the recorded drain",
        )?;
        self.activate_gateway(&binary, &facts, intent, &new)
    }

    fn drain_gateway(
        &self,
        intent: &GatewayIntent,
        drain: &mut Fence,
        destination: &Path,
        loaded: &Service,
    ) -> CliResult<bool> {
        drain.active()?;
        check(
            self.gateway_service(destination)? == *loaded,
            "candidate gateway service or credential custody differs; use the manual procedure",
        )?;
        // Persist the one-shot claim before dispatch. A lost reply is never retried.
        drain.claim_once()?;
        crate::gateway::deploy_drain(&self.host.gateway_socket, &intent.source.holder.instance)
    }

    fn activate_gateway(
        &self,
        binary: &Path,
        facts: &Facts,
        intent: &GatewayIntent,
        new: &RunnerLink,
    ) -> CliResult<GatewayOutcome> {
        let loaded = self.await_gateway_exit(binary, intent, new)?;
        self.gateway_source_host(intent)?;
        check(
            self.checked_gateway_service(binary, intent, new)? == loaded
                && crate::gateway::deploy_holder_released(&loaded.state_root)?,
            "credential holder or service changed before link flip",
        )?;
        if loaded.link != *new {
            snapshot_install::atomic_link(&loaded.command, &new.path)?;
            fs::File::open(snapshot_install::parent(&loaded.command)?)
                .and_then(|dir| dir.sync_all())
                .map_err(io)?;
        }
        let loaded = self.checked_gateway_service(binary, intent, new)?;
        check(
            loaded.state == State::CleanExit && loaded.link == *new,
            "gateway service or command changed after link flip",
        )?;
        self.start_gateway()?;
        let deadline = Instant::now() + Duration::from_secs(timing::GATEWAY_WAIT);
        loop {
            if fs::symlink_metadata(&self.host.gateway_socket)
                .is_ok_and(|meta| meta.file_type().is_socket())
                && let Ok((observed, _)) = self.observe_gateway(binary, &facts.gateway_spoken)
            {
                verify_successor(intent, new, &observed)?;
                self.gateway_source_host(intent)?;
                return Ok(GatewayOutcome::Ready { observed });
            }
            check(
                Instant::now() < deadline,
                "new gateway holder is not ready; inspect service before host handoff",
            )?;
            thread::sleep(Duration::from_millis(200));
        }
    }

    fn gateway_source_host(&self, intent: &GatewayIntent) -> CliResult<PathBuf> {
        let images = self.host_images()?;
        let instance = self.identity()?;
        check(
            images.stage_identity == intent.target
                && images.sha256 == intent.source_host
                && images.link_sha256 == intent.source_host
                && instance.instance == intent.source_instance,
            "recorded source host or stage identity changed; do not move the gateway link",
        )?;
        self.process_image(&self.running_host_pid()?)
    }

    fn await_gateway_exit(
        &self,
        binary: &Path,
        intent: &GatewayIntent,
        new: &RunnerLink,
    ) -> CliResult<Service> {
        let deadline = Instant::now() + Duration::from_secs(timing::GATEWAY_WAIT);
        loop {
            match self.gateway_manager(&intent.source.command)?.1 {
                State::CleanExit => {
                    let service = self.checked_gateway_service(binary, intent, new)?;
                    check(
                        service.state == State::CleanExit,
                        "gateway was no longer inactive after its drain",
                    )?;
                    return Ok(service);
                }
                State::Running(pid) => check(
                    pid == intent.source.holder.pid,
                    "unexpected running gateway during drain",
                )?,
                State::Transition => {}
            }
            check(
                Instant::now() < deadline,
                "gateway did not exit cleanly after drain; inspect service",
            )?;
            thread::sleep(Duration::from_millis(200));
        }
    }

    fn checked_gateway_service(
        &self,
        binary: &Path,
        intent: &GatewayIntent,
        new: &RunnerLink,
    ) -> CliResult<Service> {
        let service = self.gateway_service(binary)?;
        check(
            service.custody == intent.source.custody_sha256
                && service.command == intent.source.command
                && (service.link == intent.source.link || service.link == *new),
            "gateway service, custody or dedicated link changed; inspect before continuing",
        )?;
        Ok(service)
    }

    fn gateway_service(&self, binary: &Path) -> CliResult<Service> {
        let link = self.host.node.home.join(".local/bin/plakd-gateway");
        snapshot_install::require_owned(snapshot_install::parent(&link)?)?;
        let image = self.gateway_image(&fs::read_link(&link).map_err(|_| fail(NORMALIZE))?)?;
        let (unit, state) = self.gateway_manager(&link)?;
        let configured =
            self.product(binary, ["gateway", "install", "--dry-run"], timing::PROBE)?;
        let expected = format!("# {}\n", unit.display());
        let body = shipping::read(&unit, shipping::LIMIT)?;
        check(
            configured.strip_prefix(expected.as_bytes()) == Some(body.as_slice()),
            NORMALIZE,
        )?;
        let state_root = self.gateway_state_root(binary)?;
        let configs = [
            self.host.product_home.join("config.toml"),
            self.host.product_home.join("gateway.toml"),
            self.host.node.home.join(".config/plak/gateway.env"),
        ]
        .iter()
        .map(|path| {
            if fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                return Ok(None);
            }
            shipping::read(path, shipping::LIMIT).map(|bytes| Some(shipping::digest(&bytes)))
        })
        .collect::<CliResult<Vec<_>>>()?;
        let custody = shipping::canonical(&(
            configs,
            shipping::digest(&body),
            &state_root,
            &self.host.gateway_socket,
        ))?;
        Ok(Service {
            command: link,
            link: image,
            custody: shipping::digest(&custody),
            state_root,
            state,
        })
    }

    fn gateway_manager(&self, command: &Path) -> CliResult<(PathBuf, State)> {
        let label = &self.host.gateway_unit;
        if self.host.node.platform != Platform::Mac {
            return self.gateway_systemd(label, command);
        }
        let unit = self.plist(label);
        let loaded = launchd::observe_service(self, label, &unit)?;
        check(
            loaded.program == command
                && loaded.arguments.len() == 3
                && Path::new(&loaded.arguments[0]) == command
                && loaded.arguments[1..] == ["gateway", "serve"],
            NORMALIZE,
        )?;
        let state = loaded.pid.map_or_else(
            || {
                if loaded.clean_exit {
                    State::CleanExit
                } else {
                    State::Transition
                }
            },
            State::Running,
        );
        Ok((unit, state))
    }

    fn gateway_state_root(&self, binary: &Path) -> CliResult<PathBuf> {
        let admin = self.product(binary, ["gateway", "status", "--dry-run"], timing::PROBE)?;
        let admin = text(&admin)?;
        let expected = format!("socket {}", self.host.gateway_socket.display());
        check(admin.lines().any(|line| line == expected), CUSTODY)?;
        let state = admin
            .lines()
            .find_map(|line| line.strip_prefix("state_root "))
            .map(PathBuf::from)
            .ok_or_else(|| fail(CUSTODY))?;
        inventory::absolute(&state)?;
        Ok(state)
    }

    fn gateway_systemd(&self, unit: &str, link: &Path) -> CliResult<(PathBuf, State)> {
        let props = self.properties(
            unit,
            &[
                "LoadState",
                "ActiveState",
                "SubState",
                "MainPID",
                "ExecStart",
                "NeedDaemonReload",
                "FragmentPath",
                "DropInPaths",
                "Result",
                "ExecMainStatus",
            ],
        )?;
        let at = |key| props.get(key).map(String::as_str);
        let exec = at("ExecStart")
            .and_then(|row| row.strip_prefix("{ path="))
            .and_then(|row| row.split_once(" ; argv[]="))
            .and_then(|(path, rest)| rest.split_once(" ; ").map(|(args, _)| (path, args)));
        let fragment = self.host.node.home.join(".config/systemd/user").join(unit);
        check(
            exec.is_some_and(|(path, args)| {
                Path::new(path) == link && args == format!("{} gateway serve", link.display())
            }) && at("NeedDaemonReload") == Some("no")
                && at("LoadState") == Some("loaded")
                && at("DropInPaths") == Some("")
                && at("FragmentPath").map(Path::new) == Some(&fragment),
            NORMALIZE,
        )?;
        let state = match at("ActiveState") {
            Some("active") => State::Running(active_pid(&props)?.to_owned()),
            Some("inactive")
                if at("SubState") == Some("dead")
                    && at("MainPID") == Some("0")
                    && at("Result") == Some("success")
                    && at("ExecMainStatus") == Some("0") =>
            {
                State::CleanExit
            }
            Some("activating" | "deactivating") => State::Transition,
            _ => {
                return Err(fail(
                    "gateway service is not running or cleanly inactive; inspect service",
                ));
            }
        };
        Ok((fragment, state))
    }

    fn gateway_image(&self, path: &Path) -> CliResult<RunnerLink> {
        snapshot_install::require_owned(snapshot_install::parent(path)?)?;
        let stat = fs::symlink_metadata(path).map_err(io)?;
        check(
            self.snapshot_binary(path)
                && stat.is_file()
                && stat.uid() == self.host.node.uid
                && stat.mode() & 0o7777 == 0o555,
            "gateway command must point directly to an owned immutable plakd snapshot",
        )?;
        Ok(RunnerLink {
            path: path.to_owned(),
            sha256: FileFacts::read(path)?.sha256,
        })
    }

    fn start_gateway(&self) -> CliResult<()> {
        let cmd = if self.host.node.platform == Platform::Mac {
            let mut cmd = command(
                "/bin/launchctl".as_ref(),
                &self.host.node,
                &self.host.product_home,
            );
            cmd.args([
                "kickstart",
                &format!("gui/{}/{}", self.host.node.uid, self.host.gateway_unit),
            ]);
            cmd
        } else {
            let mut cmd = command(
                "systemctl".as_ref(),
                &self.host.node,
                &self.host.product_home,
            );
            cmd.args(["--user", "start", &self.host.gateway_unit]);
            cmd
        };
        capture(cmd, timing::PROBE)?;
        Ok(())
    }
}

fn negotiate(spoken: &[u32], served: &[u32]) -> Option<u32> {
    spoken.iter().copied().filter(|v| served.contains(v)).max()
}

fn verify_successor(
    intent: &GatewayIntent,
    new: &RunnerLink,
    observed: &GatewayObserved,
) -> CliResult<()> {
    check(
        observed.command == intent.source.command
            && observed.custody_sha256 == intent.source.custody_sha256
            && observed.link == *new
            && observed.holder.instance != intent.source.holder.instance
            && observed.holder.versions == intent.served
            && observed
                .holder
                .negotiated
                .is_some_and(|v| v >= intent.minimum),
        "gateway changed, but the new holder, image, custody or negotiated protocol could not be proved",
    )
}
