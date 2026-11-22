use std::fs::File;
use std::io::{Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::CliResult;
use crate::operator_process;
use crate::shipping_artifact::{self as shipping, FileFacts, check, fail, io};

use super::inventory::{Node, Platform, Stage};
use super::protocol::{Call, MaintainerBuild, Reply, Request, VERSION};

// Fixed loaders; the path, size and digest are data, never shell arguments.
const RUN: &str = r#"sh -c '
set -eu
PATH=/usr/bin:/bin
export PATH
IFS= read -r binary
IFS= read -r digest
case "$(uname -s)" in
  Darwin) actual=$(/usr/bin/shasum -a 256 <"$binary") ;;
  Linux) actual=$(/usr/bin/sha256sum <"$binary") ;;
  *) exit 1 ;;
esac
test "${actual%% *}" = "$digest"
exec "$binary" deploy-target
'"#;
// This loader has only one effect: a private bootstrap file. It verifies the
// operator-pinned digest before executing the temporary image with stdin intact.
const BOOT: &str = r#"sh -c '
set -eu
PATH=/usr/bin:/bin
export PATH
umask 077
IFS= read -r size
IFS= read -r digest
work=$(mktemp -d /tmp/plakd-deploy.XXXXXXXX)
trap "rm -rf -- \"$work\"" 0
trap "exit 1" HUP INT TERM
head -c "$size" >"$work/plakd"
test "$(wc -c <"$work/plakd" | tr -d " ")" = "$size"
case "$(uname -s)" in
  Darwin) actual=$(/usr/bin/shasum -a 256 <"$work/plakd") ;;
  Linux) actual=$(/usr/bin/sha256sum <"$work/plakd") ;;
  *) exit 1 ;;
esac
test "${actual%% *}" = "$digest"
chmod 0500 "$work/plakd"
"$work/plakd" deploy-target
'"#;

pub(super) enum Image<'a> {
    Installed(&'a Path, &'a FileFacts),
    Transfer(&'a Path, &'a FileFacts),
}

pub(super) struct Blob<'a> {
    pub(super) path: &'a Path,
    pub(super) facts: &'a FileFacts,
}

pub(super) struct Ssh {
    operator_machine: Option<String>,
    stage: Stage,
}

impl Ssh {
    pub(super) fn new(stage: Stage) -> CliResult<Self> {
        let operator_machine = Platform::native().map(machine_id).transpose()?;
        Ok(Self {
            operator_machine,
            stage,
        })
    }

    pub(super) fn require_glibc(node: &Node, ceiling: Option<[u32; 3]>) -> CliResult<()> {
        let Some(ceiling) = ceiling else {
            return Ok(());
        };
        let output = run(
            node,
            "exec /usr/bin/getconf GNU_LIBC_VERSION",
            tempfile::tempfile().map_err(io)?,
            super::timing::LOGIN,
        )
        .map_err(|_| fail("cannot read target GLIBC version with getconf"))?;
        let version = super::probe::text(&output)?
            .trim()
            .strip_prefix("glibc ")
            .ok_or_else(|| fail("target did not report a GNU libc version"))?;
        let version = crate::snapshot_install::maintainer::glibc_version(version)?;
        check(
            version >= ceiling,
            &format!(
                "target GLIBC {}.{}.{} is older than required {}.{}.{}",
                version[0], version[1], version[2], ceiling[0], ceiling[1], ceiling[2],
            ),
        )
    }

    pub(super) fn call<T: DeserializeOwned>(
        &self,
        node: &Node,
        image: Image<'_>,
        call: Call,
        maintainer_build: Option<MaintainerBuild>,
        files: &[Blob<'_>],
        timeout: u64,
    ) -> CliResult<T> {
        let mut input = tempfile::tempfile().map_err(io)?;
        let command = match image {
            Image::Installed(path, expected) => {
                writeln!(input, "{}\n{}", path.display(), expected.sha256).map_err(io)?;
                RUN
            }
            Image::Transfer(path, expected) => {
                writeln!(input, "{}\n{}", expected.length, expected.sha256).map_err(io)?;
                shipping::copy(path, &mut input, expected)?;
                BOOT
            }
        };
        let request = Request {
            deploy_protocol: VERSION,
            operator_machine: self.operator_machine.clone(),
            stage: self.stage,
            maintainer_build,
            call,
        };
        let request = shipping::canonical(&request)?;
        check(
            request.len() as u64 <= shipping::LIMIT,
            "SSH request exceeds its bound",
        )?;
        input.write_all(&request).map_err(io)?;
        for file in files {
            shipping::copy(file.path, &mut input, file.facts)?;
        }
        let output = run(node, command, input, timeout)?;
        match serde_json::from_slice(&output)
            .map_err(|_| fail("SSH target did not return a typed verdict"))?
        {
            Reply::Ok(value) => serde_json::from_value(value)
                .map_err(|_| fail("SSH target returned unexpected facts")),
            Reply::Error(reason) => Err(fail(&reason)),
            Reply::ProtocolMismatch { supported, remedy } => Err(fail(&format!(
                "deploy protocol mismatch: operator v{VERSION}, target v{supported}; {remedy}"
            ))),
        }
    }
}

fn run(node: &Node, script: &str, mut input: File, timeout: u64) -> CliResult<Vec<u8>> {
    input.rewind().map_err(io)?;
    let mut command = operator_process::command("ssh".as_ref());
    command.args([
        "-T",
        "-F",
        "/dev/null",
        "-oBatchMode=yes",
        "-oControlMaster=no",
        "-oControlPath=none",
        "-oStrictHostKeyChecking=yes",
        "-oConnectTimeout=10",
        "-oServerAliveInterval=5",
        "-oServerAliveCountMax=2",
        "-oForwardAgent=no",
        "-oClearAllForwardings=yes",
        "-l",
        &node.login,
        "--",
        &node.ssh,
        script,
    ]);
    let output = operator_process::capture(
        command,
        Some(input),
        Duration::from_secs(timeout),
        shipping::LIMIT,
    )
    .map_err(|_| fail("SSH exchange was unavailable or exceeded its bound"))?;
    if !output.status.success() {
        return Err(fail(&format!(
            "SSH returned no bounded verdict ({})",
            stderr_class(&output.stderr)
        )));
    }
    Ok(output.stdout)
}

fn stderr_class(bytes: &[u8]) -> &'static str {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    [
        ("host key verification failed", "host_key"),
        ("remote host identification has changed", "host_key"),
        ("permission denied", "authentication"),
        ("could not resolve hostname", "name_resolution"),
        ("connection refused", "connection_refused"),
        ("timed out", "timeout"),
        ("broken pipe", "transport_lost"),
        ("closed by remote host", "transport_lost"),
    ]
    .into_iter()
    .find_map(|(needle, kind)| text.contains(needle).then_some(kind))
    .unwrap_or("remote_command_or_transport")
}

pub(super) fn unreachable(error: &crate::CliFailure) -> bool {
    matches!(
        error.message(),
        "SSH exchange was unavailable or exceeded its bound"
            | "SSH returned no bounded verdict (name_resolution)"
            | "SSH returned no bounded verdict (connection_refused)"
            | "SSH returned no bounded verdict (timeout)"
            | "SSH returned no bounded verdict (transport_lost)"
    )
}

pub(super) fn machine_id(platform: Platform) -> CliResult<String> {
    if platform == Platform::Mac {
        let mut command = operator_process::command("/usr/sbin/ioreg".as_ref());
        command
            .env_clear()
            .args(["-rd1", "-c", "IOPlatformExpertDevice"]);
        let raw = super::probe::capture(command, super::timing::LOGIN)?;
        return mac_machine_id(super::probe::text(&raw)?);
    }
    let id =
        std::fs::read("/etc/machine-id").map_err(|_| fail("cannot establish machine identity"))?;
    let id = std::str::from_utf8(&id)
        .map_err(|_| fail("invalid machine identity"))?
        .trim();
    check(shipping::hex(id, 32), "invalid machine identity")?;
    Ok(shipping::digest(id.as_bytes()))
}

fn mac_machine_id(output: &str) -> CliResult<String> {
    let mut ids = output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("\"IOPlatformUUID\" = "));
    let invalid = || fail("invalid macOS machine identity");
    let id = ids
        .next()
        .ok_or_else(|| fail("cannot establish macOS machine identity"))?;
    check(ids.next().is_none(), "duplicate macOS machine identity")?;
    let id = id
        .strip_prefix('"')
        .and_then(|id| id.strip_suffix('"'))
        .ok_or_else(invalid)?;
    let id = uuid::Uuid::parse_str(id).map_err(|_| invalid())?;
    check(!id.is_nil(), "invalid macOS machine identity")?;
    Ok(shipping::digest(id.as_bytes()))
}

pub(super) fn stable(node: &Node) -> PathBuf {
    node.home.join(".local/bin/plakd")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_mac_machine_id_uses_one_non_nil_platform_uuid() {
        let uuid = "9BA92390-447E-4368-976C-7FAE72AB0021";
        let row = format!("    \"IOPlatformUUID\" = \"{uuid}\"");
        let result = mac_machine_id(&format!("+-o IOPlatformExpertDevice\n{{\n{row}\n}}")).unwrap();
        assert_eq!(
            result,
            shipping::digest(uuid::Uuid::parse_str(uuid).unwrap().as_bytes())
        );
        for invalid in [
            String::new(),
            format!("{row}\n{row}"),
            format!("{row}\n\"IOPlatformUUID\" = \"incomplete"),
            row.replace(uuid, "not-a-uuid"),
            row.replace(uuid, "00000000-0000-0000-0000-000000000000"),
        ] {
            assert!(mac_machine_id(&invalid).is_err());
        }
    }
}
