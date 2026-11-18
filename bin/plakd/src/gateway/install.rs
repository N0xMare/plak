//! `plakd gateway install` writes a user service definition and does not start it.
//! The unit runs whatever executable is installing it; the cutover runbook pins
//! that path to an immutable snapshot before the service is enabled.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub(crate) fn install(executable: &Path, dry_run: bool) -> Result<PathBuf, String> {
    let executable = executable
        .to_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| "the gateway executable path is not UTF-8".to_owned())?;
    if cfg!(target_os = "linux") {
        install_systemd(executable, dry_run)
    } else if cfg!(target_os = "macos") {
        install_launchd(executable, dry_run)
    } else {
        Err("plakd gateway install supports Linux user units and macOS LaunchAgents".to_owned())
    }
}

pub(crate) fn uninstall() -> Result<String, String> {
    if cfg!(target_os = "linux") {
        remove_definition(&systemd_path()?, "no unit at")
    } else if cfg!(target_os = "macos") {
        remove_definition(&launchd_path()?, "no LaunchAgent at")
    } else {
        Err("plakd gateway uninstall supports Linux user units and macOS LaunchAgents".to_owned())
    }
}

fn remove_definition(path: &Path, absent: &str) -> Result<String, String> {
    if path.is_file() {
        fs::remove_file(path).map_err(|error| error.to_string())?;
        Ok(format!("removed {}", path.display()))
    } else {
        Ok(format!("{absent} {}", path.display()))
    }
}

fn install_systemd(executable: &str, dry_run: bool) -> Result<PathBuf, String> {
    let path = systemd_path()?;
    let unit = systemd_unit(executable);
    if dry_run {
        print_dry(&path, &unit)?;
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    write_new(&path, &unit)?;
    Ok(path)
}

fn install_launchd(executable: &str, dry_run: bool) -> Result<PathBuf, String> {
    let path = launchd_path()?;
    let plist = launchd_plist(executable);
    if dry_run {
        print_dry(&path, &plist)?;
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    write_new(&path, &plist)?;
    Ok(path)
}

fn systemd_path() -> Result<PathBuf, String> {
    let home =
        crate::environment::inherited_home().ok_or_else(|| "HOME is unavailable".to_owned())?;
    let config = crate::environment::xdg_config_home()
        .unwrap_or_else(|| PathBuf::from(home).join(".config"));
    Ok(config.join("systemd/user/plak-gateway.service"))
}

fn launchd_path() -> Result<PathBuf, String> {
    let home =
        crate::environment::inherited_home().ok_or_else(|| "HOME is unavailable".to_owned())?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents/com.plakd.gateway.plist"))
}

pub(crate) fn systemd_unit(executable: &str) -> String {
    format!(
        "\
[Unit]
Description=plakd credential gateway
[Service]
Type=simple
ExecStart={executable} gateway serve
# EnvironmentFile=-%h/.config/plak/gateway.env
Restart=on-failure
[Install]
WantedBy=default.target
"
    )
}

fn launchd_plist(executable: &str) -> String {
    format!(
        "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
  <key>Label</key>
  <string>com.plakd.gateway</string>
  <key>ProgramArguments</key>
  <array>
    <string>{executable}</string>
    <string>gateway</string>
    <string>serve</string>
  </array>
  <key>RunAtLoad</key>
  <false/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
</dict>
</plist>
"
    )
}

fn write_new(path: &Path, body: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "{} already exists; it was not replaced",
            path.display()
        ));
    }
    let mut file = fs::File::create(path).map_err(|error| error.to_string())?;
    file.write_all(body.as_bytes())
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn print_dry(path: &Path, body: &str) -> Result<(), String> {
    let mut out = std::io::stdout();
    writeln!(out, "# {}", path.display()).map_err(|error| error.to_string())?;
    writeln!(out, "{body}").map_err(|error| error.to_string())?;
    Ok(())
}
