//! The credential gateway's user service and independently pinned command link.

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use crate::commands::host_install::{matching_unit, publish_unit};
use crate::home::paths::PlakPaths;

pub(crate) const SYSTEMD_UNIT: &str = "plak-gateway.service";
pub(crate) const LAUNCHD_LABEL: &str = "com.plakd.gateway";
const LAUNCHD_PLIST: &str = "com.plakd.gateway.plist";

pub(crate) struct GatewayUnit {
    pub(crate) path: PathBuf,
    pub(crate) body: String,
    link: PathBuf,
}

impl GatewayUnit {
    pub(crate) fn render(
        user_home: &Path,
        product_home: &Path,
        state_home: &Path,
    ) -> Result<Self, String> {
        let runtime = if cfg!(target_os = "macos") {
            crate::environment::xdg_runtime_dir().filter(|path| path.is_absolute())
        } else {
            None
        };
        Self::for_platform(
            user_home,
            product_home,
            state_home,
            runtime.as_deref(),
            cfg!(target_os = "macos"),
        )
    }

    fn for_platform(
        user_home: &Path,
        product_home: &Path,
        state_home: &Path,
        runtime: Option<&Path>,
        mac: bool,
    ) -> Result<Self, String> {
        let link = user_home.join(".local/bin/plakd-gateway");
        let executable = text(&link)?;
        let homes = [
            ("HOME", text(user_home)?),
            ("PLAK_HOME", text(product_home)?),
            ("XDG_STATE_HOME", text(state_home)?),
            (
                "XDG_RUNTIME_DIR",
                runtime.map(text).transpose()?.unwrap_or_default(),
            ),
        ];
        let body = if mac {
            launchd_plist(executable, &homes)
        } else {
            systemd_unit(executable, &homes[..3])
        };
        Ok(Self {
            path: if mac {
                launchd_path(user_home)
            } else {
                systemd_path(user_home)
            },
            body,
            link,
        })
    }

    pub(crate) fn check(&self, installer: &str) -> io::Result<()> {
        matching_unit(&self.path, &self.body, installer)?;
        check_link(&self.link).map(|_| ())
    }

    pub(crate) fn install(&self, installer: &str) -> io::Result<bool> {
        let present = matching_unit(&self.path, &self.body, installer)?;
        ensure_link(&self.link, || std::env::current_exe()?.canonicalize())?;
        if !present {
            publish_unit(&self.path, &self.body)?;
        }
        Ok(!present)
    }
}

fn check_link(link: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(link) {
        Ok(metadata)
            if metadata.is_symlink()
                && fs::metadata(link).is_ok_and(|target| {
                    target.is_file() && target.permissions().mode() & 0o111 != 0
                }) =>
        {
            Ok(true)
        }
        Ok(_) => Err(io::Error::other(format!(
            "refusing to replace {}; point it at an installed, executable plakd snapshot, then retry",
            link.display(),
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn ensure_link(link: &Path, executable: impl FnOnce() -> io::Result<PathBuf>) -> io::Result<()> {
    if check_link(link)? {
        return Ok(());
    }
    let target = executable()?;
    let parent = link
        .parent()
        .ok_or_else(|| io::Error::other("gateway link has no parent"))?;
    fs::create_dir_all(parent)?;
    symlink(target, link)?;
    fs::File::open(parent)?.sync_all()
}

pub(super) fn install(
    home: &PlakPaths,
    state_home: &Path,
    dry_run: bool,
    out: &mut dyn io::Write,
) -> Result<(), String> {
    let user_home = user_home()?;
    let unit = GatewayUnit::render(&user_home, home.home(), state_home)?;
    let result = if dry_run {
        writeln!(out, "# {}\n{}", unit.path.display(), unit.body)
    } else {
        let created = unit.install("gateway").map_err(|error| error.to_string())?;
        writeln!(
            out,
            "{} {}",
            if created {
                "Installed"
            } else {
                "Already installed"
            },
            unit.path.display()
        )
    };
    result.map_err(|error| error.to_string())?;
    if cfg!(target_os = "macos") {
        write!(
            out,
            "{}",
            launchd_start(&unit.path, rustix::process::getuid().as_raw())
        )
    } else {
        writeln!(
            out,
            "systemctl --user daemon-reload\nsystemctl --user enable --now {SYSTEMD_UNIT}"
        )
    }
    .map_err(|error| error.to_string())
}

pub(super) fn uninstall() -> Result<String, String> {
    let home = user_home()?;
    let path = if cfg!(target_os = "macos") {
        launchd_path(&home)
    } else {
        systemd_path(&home)
    };
    match fs::remove_file(&path) {
        Ok(()) => Ok(format!("removed {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(format!("no unit at {}", path.display()))
        }
        Err(error) => Err(error.to_string()),
    }
}

fn user_home() -> Result<PathBuf, String> {
    crate::environment::inherited_home()
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is unavailable".to_owned())
}

fn systemd_path(home: &Path) -> PathBuf {
    crate::environment::xdg_config_home()
        .unwrap_or_else(|| home.join(".config"))
        .join("systemd/user")
        .join(SYSTEMD_UNIT)
}

fn launchd_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents").join(LAUNCHD_PLIST)
}

pub(crate) fn launchd_start(path: &Path, uid: u32) -> String {
    let path = shell_words::quote(path.to_str().unwrap_or_default());
    format!("launchctl bootstrap gui/{uid} {path}\n")
}

fn text(path: &Path) -> Result<&str, String> {
    path.to_str()
        .filter(|value| {
            path.is_absolute()
                && !value.chars().any(char::is_control)
                && !value.contains(['\u{fffe}', '\u{ffff}'])
        })
        .ok_or_else(|| {
            "gateway service paths must be absolute UTF-8 paths without control characters"
                .to_owned()
        })
}

fn systemd_unit(executable: &str, homes: &[(&str, &str)]) -> String {
    let environment: String = homes
        .iter()
        .map(|(name, value)| format!("Environment=\"{name}={}\"\n", systemd_quoted(value)))
        .collect();
    format!(
        "# Generated by plakd gateway install. Do not edit.\n\
         [Unit]\nDescription=plakd credential gateway\n\
         [Service]\nType=simple\nExecStart=:\"{}\" gateway serve\nUMask=0077\n\
         {environment}Environment=XDG_RUNTIME_DIR=%t\nEnvironmentFile=-%h/.config/plak/gateway.env\n\
         Restart=on-failure\nRestartSec=2\nTimeoutStopSec=45\n\
         [Install]\nWantedBy=default.target\n",
        systemd_quoted(executable)
    )
}

fn systemd_quoted(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
}

fn launchd_plist(executable: &str, homes: &[(&str, &str)]) -> String {
    let environment: String = homes
        .iter()
        .map(|(name, value)| format!("    <key>{name}</key><string>{}</string>\n", xml(value)))
        .collect();
    let executable = xml(executable);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- Generated by plakd gateway install. Do not edit. -->
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{executable}</string>
    <string>gateway</string>
    <string>serve</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
{environment}  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ExitTimeOut</key>
  <integer>45</integer>
  <key>Umask</key>
  <integer>63</integer>
</dict>
</plist>
"#
    )
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_renders_both_platform_goldens() {
        let homes = [
            ("HOME", "/fixture/a &<>%\"\\"),
            ("PLAK_HOME", "/fixture/a &<>%\"\\/product"),
            ("XDG_STATE_HOME", "/fixture/a &<>%\"\\/state"),
        ];
        let executable = "/fixture/a &<>%\"\\/.local/bin/plakd-gateway";
        assert_eq!(
            systemd_unit(executable, &homes),
            include_str!("../../tests/goldens/plak-gateway.service")
        );
        let homes = homes
            .into_iter()
            .chain([("XDG_RUNTIME_DIR", "/private/run &<>")])
            .collect::<Vec<_>>();
        assert_eq!(
            launchd_plist(executable, &homes),
            include_str!("../../tests/goldens/com.plakd.gateway.plist")
        );
    }
}
