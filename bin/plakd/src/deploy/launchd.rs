use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use crate::CliResult;
use crate::shipping_artifact::{check, fail, io};

use super::probe::{self, Target};
use super::timing;

pub(super) struct Agent {
    pub(super) pid: String,
    pub(super) program: PathBuf,
}

pub(super) fn observe(target: &Target<'_>, label: &str, plist: &Path) -> CliResult<Agent> {
    let node = &target.host.node;
    crate::snapshot_install::require_owned(crate::snapshot_install::parent(plist)?)?;
    let mut manager = probe::command("/bin/launchctl".as_ref(), node, &target.host.product_home);
    let service = format!("gui/{}/{label}", node.uid);
    manager.args(["print", &service]);
    let loaded = probe::capture(manager, timing::PROBE)?;
    let mut tool = probe::command("/usr/bin/plutil".as_ref(), node, &target.host.product_home);
    tool.args(["-extract", "ProgramArguments", "json", "-o", "-", "--"])
        .arg(plist);
    let args: Vec<String> = probe::decode(&probe::capture(tool, timing::PROBE)?)?;
    parse(probe::text(&loaded)?, &service, plist, &args)
}

fn parse(output: &str, service: &str, plist: &Path, args: &[String]) -> CliResult<Agent> {
    let mut lines = output.lines();
    check(
        lines.next() == Some(&format!("{service} = {{")),
        "unexpected LaunchAgent identity",
    )?;
    let mut props = BTreeMap::new();
    let mut arguments = None;
    while let Some(line) = lines.next() {
        let Some(line) = line
            .strip_prefix('\t')
            .filter(|line| !line.starts_with('\t'))
        else {
            continue;
        };
        if line == "arguments = {" {
            let values = lines
                .by_ref()
                .take_while(|line| *line != "\t}")
                .map(|line| {
                    line.strip_prefix("\t\t")
                        .ok_or_else(|| fail("invalid LaunchAgent arguments"))
                })
                .collect::<CliResult<Vec<_>>>()?;
            check(
                arguments.replace(values).is_none(),
                "duplicate LaunchAgent arguments",
            )?;
        } else if let Some((key, value)) = line.split_once(" = ")
            && matches!(key, "path" | "type" | "state" | "program" | "pid")
        {
            check(
                props.insert(key, value).is_none(),
                "duplicate LaunchAgent property",
            )?;
        }
    }
    check(
        props.get("path").map(Path::new) == Some(plist)
            && props.get("type") == Some(&"LaunchAgent")
            && props.get("state") == Some(&"running")
            && arguments
                .as_deref()
                .is_some_and(|loaded| loaded.iter().copied().eq(args.iter().map(String::as_str))),
        "expected the running LaunchAgent and its on-disk arguments; check launchctl print without restarting",
    )?;
    let program = props
        .get("program")
        .copied()
        .or_else(|| args.first().map(String::as_str))
        .ok_or_else(|| fail("missing LaunchAgent program"))?;
    super::inventory::absolute(Path::new(program))?;
    let pid = props
        .get("pid")
        .filter(|pid| pid.parse::<u32>().is_ok_and(|pid| pid > 0))
        .ok_or_else(|| fail("missing launchd PID"))?;
    Ok(Agent {
        pid: (*pid).to_owned(),
        program: program.into(),
    })
}

pub(super) fn process_image(target: &Target<'_>, pid: &str) -> CliResult<PathBuf> {
    let mut tool = probe::command(
        "/usr/sbin/lsof".as_ref(),
        &target.host.node,
        &target.host.product_home,
    );
    tool.args(["-nP", "-a", "-p", pid, "-d", "txt", "-FpfDin"]);
    let raw = probe::capture(tool, timing::PROBE)?;
    let (path, device, inode) = executable(probe::text(&raw)?, pid)?;
    let path = path.canonicalize().map_err(io)?;
    let meta = fs::metadata(&path).map_err(io)?;
    check(
        meta.is_file() && meta.dev() == device && meta.ino() == inode,
        "running executable vnode differs from the image on disk",
    )?;
    Ok(path)
}

fn executable(output: &str, pid: &str) -> CliResult<(PathBuf, u64, u64)> {
    let mut lines = output.lines();
    let invalid = || fail("cannot prove the LaunchAgent executable vnode");
    check(
        lines.next().and_then(|line| line.strip_prefix('p')) == Some(pid)
            && lines.next() == Some("ftxt"),
        "cannot identify the LaunchAgent process image",
    )?;
    // Darwin lsof lists the executable vnode first, before mapped libraries.
    let fields: BTreeMap<_, _> = lines
        .take_while(|line| !line.starts_with(['f', 'p']))
        .filter_map(|line| line.split_at_checked(1))
        .collect();
    let device = u64::from_str_radix(
        fields
            .get("D")
            .and_then(|line| line.strip_prefix("0x"))
            .ok_or_else(invalid)?,
        16,
    )
    .map_err(|_| invalid())?;
    let inode = fields
        .get("i")
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    let path = PathBuf::from(fields.get("n").ok_or_else(invalid)?);
    super::inventory::absolute(&path)?;
    Ok((path, device, inode))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_launchagent_proves_loaded_arguments_and_executable_vnode() {
        let service = "gui/500/com.example";
        let plist =
            Path::new("/Volumes/example/example-user/Library/LaunchAgents/com.example.plist");
        let loaded = format!(
            "{service} = {{\n\tpath = {}\n\ttype = LaunchAgent\n\tstate = running\n\tprogram = /example/link\n\targuments = {{\n\t\t/example/link\n\t\tgateway\n\t}}\n\tpid = 42\n}}\n",
            plist.display()
        );
        let args = ["/example/link".into(), "gateway".into()];
        let agent = parse(&loaded, service, plist, &args).unwrap();
        assert_eq!(agent.pid, "42");
        assert_eq!(agent.program, Path::new("/example/link"));
        assert!(parse(&loaded, service, plist, &["/other".into()]).is_err());
        assert!(parse(&loaded.replace("running", "waiting"), service, plist, &args).is_err());
        let images =
            "p42\nftxt\nD0xa\ni100\nn/example/snapshot/plakd\nftxt\nD0xf\ni200\nn/usr/lib/dyld\n";
        assert_eq!(
            executable(images, "42").unwrap(),
            ("/example/snapshot/plakd".into(), 10, 100)
        );
        assert!(executable(images, "43").is_err());
    }
}
