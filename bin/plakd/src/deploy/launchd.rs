use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::CliResult;
use crate::shipping_artifact::{check, fail, io};

use super::probe::{self, Target};
use super::timing;

pub(super) struct Agent {
    pub(super) pid: String,
    pub(super) program: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Plist {
    program_arguments: Vec<String>,
    program: Option<PathBuf>,
    working_directory: Option<String>,
    standard_out_path: Option<String>,
    standard_error_path: Option<String>,
    exit_time_out: Option<u64>,
    umask: Option<u64>,
    run_at_load: Option<bool>,
    abandon_process_group: Option<bool>,
    keep_alive: Option<serde_json::Value>,
}

pub(super) fn observe(target: &Target<'_>, label: &str, plist: &Path) -> CliResult<Agent> {
    let node = &target.host.node;
    crate::snapshot_install::require_owned(crate::snapshot_install::parent(plist)?)?;
    let (service, loaded) = print(target, label)?;
    let mut tool = probe::command("/usr/bin/plutil".as_ref(), node, &target.host.product_home);
    tool.args(["-convert", "json", "-o", "-", "--"]).arg(plist);
    let config = probe::decode(&probe::capture(tool, timing::PROBE)?)?;
    parse(probe::text(&loaded)?, &service, plist, &config)
}

pub(super) fn running_pid(target: &Target<'_>, label: &str) -> CliResult<String> {
    let (service, loaded) = print(target, label)?;
    let lines = service_lines(probe::text(&loaded)?, &service)?;
    let mut props = BTreeMap::new();
    for (key, value) in lines
        .filter_map(top_level)
        .filter_map(|line| line.split_once(" = "))
        .filter(|(key, _)| matches!(*key, "type" | "state" | "pid"))
    {
        check(
            props.insert(key, value).is_none(),
            "duplicate LaunchAgent identity property",
        )?;
    }
    active_pid(&props).map(str::to_owned)
}

fn print(target: &Target<'_>, label: &str) -> CliResult<(String, Vec<u8>)> {
    let node = &target.host.node;
    let mut manager = probe::command("/bin/launchctl".as_ref(), node, &target.host.product_home);
    let service = format!("gui/{}/{label}", node.uid);
    manager.args(["print", &service]);
    Ok((service, probe::capture(manager, timing::PROBE)?))
}

fn service_lines<'a>(output: &'a str, service: &str) -> CliResult<std::str::Lines<'a>> {
    let mut lines = output.lines();
    check(
        lines.next() == Some(&format!("{service} = {{")),
        "unexpected LaunchAgent identity",
    )?;
    Ok(lines)
}

fn active_pid<'a>(props: &BTreeMap<&str, &'a str>) -> CliResult<&'a str> {
    check(
        props.get("type") == Some(&"LaunchAgent") && props.get("state") == Some(&"running"),
        "expected an active LaunchAgent",
    )?;
    props
        .get("pid")
        .copied()
        .filter(|pid| pid.parse::<u32>().is_ok_and(|pid| pid > 0))
        .ok_or_else(|| fail("missing launchd PID"))
}

fn parse(output: &str, service: &str, plist: &Path, config: &Plist) -> CliResult<Agent> {
    let mut lines = service_lines(output, service)?;
    let mut props = BTreeMap::new();
    let mut arguments = None;
    let mut keep_alive = None;
    while let Some(line) = lines.next() {
        let Some(line) = top_level(line) else {
            continue;
        };
        if matches!(
            line,
            "arguments = {" | "keepalive conditions = {" | "keepalive criteria = {"
        ) {
            let values = lines
                .by_ref()
                .take_while(|line| *line != "\t}")
                .map(|line| {
                    line.strip_prefix("\t\t")
                        .ok_or_else(|| fail("invalid LaunchAgent arguments"))
                })
                .collect::<CliResult<Vec<_>>>()?;
            check(
                if line == "arguments = {" {
                    arguments.replace(values).is_none()
                } else {
                    keep_alive.replace(values).is_none()
                },
                "duplicate LaunchAgent block",
            )?;
        } else if let Some((key, value)) = line.split_once(" = ")
            && matches!(
                key,
                "path"
                    | "type"
                    | "state"
                    | "program"
                    | "pid"
                    | "working directory"
                    | "stdout path"
                    | "stderr path"
                    | "exit timeout"
                    | "umask"
                    | "properties"
                    | "run at load"
                    | "abandon process group"
                    | "keepalive"
            )
        {
            check(
                props.insert(key, value).is_none(),
                "duplicate LaunchAgent property",
            )?;
        }
    }
    check(
        props.get("path").map(Path::new) == Some(plist)
            && arguments.as_deref().is_some_and(|loaded| {
                loaded
                    .iter()
                    .copied()
                    .eq(config.program_arguments.iter().map(String::as_str))
            }),
        "expected the running LaunchAgent and its on-disk arguments; check launchctl print without restarting",
    )?;
    check_loaded(&props, keep_alive.as_deref().unwrap_or_default(), config)?;
    let expected_program = config
        .program
        .as_deref()
        .or_else(|| config.program_arguments.first().map(Path::new))
        .ok_or_else(|| fail("missing LaunchAgent program"))?;
    check(
        props
            .get("program")
            .is_none_or(|program| Path::new(program) == expected_program),
        "loaded LaunchAgent program differs from the plist",
    )?;
    super::inventory::absolute(expected_program)?;
    let pid = active_pid(&props)?;
    Ok(Agent {
        pid: pid.to_owned(),
        program: expected_program.into(),
    })
}

fn top_level(line: &str) -> Option<&str> {
    line.strip_prefix('\t')
        .filter(|line| !line.starts_with('\t'))
}

fn check_loaded(props: &BTreeMap<&str, &str>, criteria: &[&str], config: &Plist) -> CliResult<()> {
    let same_if_exposed = |name, expected: Option<&str>| {
        props
            .get(name)
            .zip(expected)
            .is_none_or(|(loaded, value)| *loaded == value)
    };
    let flags = |flag| {
        props
            .get("properties")
            .is_some_and(|properties| properties.split(" | ").any(|value| value == flag))
    };
    let boolean = |value| match value {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    };
    let same_bool = |label, expected: Option<bool>| {
        props
            .get(label)
            .zip(expected)
            .is_none_or(|(loaded, value)| boolean(loaded) == Some(value))
    };
    let exit_timeout = props
        .get("exit timeout")
        .zip(config.exit_time_out)
        .is_none_or(|(loaded, expected)| {
            loaded.trim_end_matches(" seconds").parse::<u64>() == Ok(expected)
        });
    let umask = props
        .get("umask")
        .zip(config.umask)
        .is_none_or(|(loaded, expected)| {
            u64::from_str_radix(loaded.trim_start_matches("0o"), 8) == Ok(expected)
        });
    let keepalive = config.keep_alive.as_ref().is_none_or(|expected| {
        if let Some(always) = expected.as_bool() {
            same_bool("keepalive", Some(always))
                && (!props.contains_key("properties") || flags("keepalive") == always)
        } else {
            let successful_exit = expected["SuccessfulExit"].as_bool();
            criteria
                .iter()
                .filter_map(|line| line.strip_prefix("successful exit = "))
                .all(|loaded| successful_exit.is_some_and(|want| boolean(loaded) == Some(want)))
                && (!flags("keepalive") || !criteria.is_empty())
                && (!props.contains_key("keepalive") || same_bool("keepalive", Some(false)))
        }
    });
    check(
        same_if_exposed("working directory", config.working_directory.as_deref())
            && same_if_exposed("stdout path", config.standard_out_path.as_deref())
            && same_if_exposed("stderr path", config.standard_error_path.as_deref())
            && exit_timeout
            && umask
            && keepalive
            && same_bool("run at load", config.run_at_load)
            && same_bool("abandon process group", config.abandon_process_group)
            && config
                .run_at_load
                .is_none_or(|want| !props.contains_key("properties") || flags("runatload") == want)
            && config.abandon_process_group.is_none_or(|want| {
                !props.contains_key("properties") || flags("abandon process group") == want
            }),
        "loaded LaunchAgent settings differ from the plist; inspect launchctl print without restarting",
    )
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
            concat!(
                "{0} = {{\n\tactive count = 1\n\tpath = {1}\n\ttype = LaunchAgent\n\tstate = running\n",
                "\tprogram = /bin/sh\n\targuments = {{\n\t\t/bin/sh\n\t\t-c\n\t\texec /example/link host\n\t}}\n",
                "\tworking directory = /example\n\tstdout path = /example/out\n\tstderr path = /example/err\n",
                "\tinherited environment = {{\n\t\tpid => ignored\n\t}}\n",
                "\tdefault environment = {{\n\t\tPATH => /usr/bin:/bin\n\t}}\n",
                "\tendpoints = {{\n\t\tapplication.example = {{\n\t\t\tstate = waiting\n\t\t}}\n\t}}\n",
                "\texit timeout = 90 seconds\n\tumask = 077\n\tpid = 42\n",
                "\tkeepalive conditions = {{\n\t\tsuccessful exit = false\n\t}}\n",
                "\tabandon process group = true\n\tproperties = runatload | abandon process group | inferred program\n}}\n"
            ),
            service,
            plist.display()
        );
        let config = serde_json::from_value(serde_json::json!({
            "ProgramArguments": ["/bin/sh","-c","exec /example/link host"], "WorkingDirectory": "/example",
            "StandardOutPath": "/example/out", "StandardErrorPath": "/example/err",
            "ExitTimeOut": 90, "Umask": 63, "RunAtLoad": true, "AbandonProcessGroup": true,
            "KeepAlive": {"SuccessfulExit": false}
        })).unwrap();
        let agent = parse(&loaded, service, plist, &config).unwrap();
        assert_eq!(agent.pid, "42");
        assert_eq!(agent.program, Path::new("/bin/sh"));
        for (before, after) in [
            ("exec /example/link host", "exec /example/other host"),
            ("state = running", "state = waiting"),
            ("working directory = /example", "working directory = /other"),
            ("stdout path = /example/out", "stdout path = /example/other"),
            ("90 seconds", "5 seconds"),
            ("umask = 077", "umask = 022"),
            ("successful exit = false", "successful exit = true"),
            (
                "abandon process group = true",
                "abandon process group = false",
            ),
            ("runatload | ", ""),
        ] {
            assert!(
                parse(&loaded.replace(before, after), service, plist, &config).is_err(),
                "{before}"
            );
        }
        let images =
            "p42\nftxt\nD0xa\ni100\nn/example/snapshot/plakd\nftxt\nD0xf\ni200\nn/usr/lib/dyld\n";
        assert_eq!(
            executable(images, "42").unwrap(),
            ("/example/snapshot/plakd".into(), 10, 100)
        );
        assert!(executable(images, "43").is_err());
    }
}
