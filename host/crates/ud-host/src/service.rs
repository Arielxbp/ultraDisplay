//! `install-service` / `uninstall-service`: a systemd user unit that starts the daemon
//! with the patched niri session. Works for the AppImage (runs `$APPIMAGE`) and for a
//! plain binary (runs the current executable).

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, bail};

const UNIT_NAME: &str = "ultradisplayd.service";
const SESSION_UNIT: &str = "niri-virtual.service";

fn unit_path() -> anyhow::Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("neither XDG_CONFIG_HOME nor HOME is set")?;
    Ok(base.join("systemd/user").join(UNIT_NAME))
}

/// The file the unit should run: the AppImage itself when running from one.
fn executable() -> anyhow::Result<PathBuf> {
    match std::env::var_os("APPIMAGE") {
        Some(p) => Ok(PathBuf::from(p)),
        None => std::env::current_exe().context("locating the ultradisplayd binary"),
    }
}

fn unit_text(exe: &str, args: &[&str]) -> String {
    let args: String = args.iter().map(|a| format!(" {a}")).collect();
    format!(
        "# Written by `ultradisplayd install-service`; remove with `uninstall-service`.\n\
         [Unit]\n\
         Description=UltraDisplay: Android tablet as a niri output\n\
         # Only meaningful inside the patched niri session (virtual outputs).\n\
         PartOf={SESSION_UNIT}\n\
         After={SESSION_UNIT}\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=\"{exe}\"{args}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy={SESSION_UNIT}\n"
    )
}

/// Whether the user unit exists.
pub fn is_installed() -> bool {
    unit_path().is_ok_and(|p| p.exists())
}

/// Writes and enables the unit, running this AppImage/binary with `args`, and starts it
/// if the patched session is running (unless `start_now` is false).
pub fn install(args: &[&str], start_now: bool) -> anyhow::Result<()> {
    let exe = executable()?;
    let exe = exe.to_str().context("executable path is not valid UTF-8")?;
    if exe.contains('"') || exe.contains('\\') || args.iter().any(|a| a.contains(['"', '\\', ' '])) {
        bail!("can't write a unit for a path containing quotes or backslashes: {exe}");
    }

    let path = unit_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, unit_text(exe, args))
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {} (ExecStart={exe})", path.display());

    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", UNIT_NAME])?;
    if start_now && systemctl(&["-q", "is-active", SESSION_UNIT]).is_ok() {
        systemctl(&["restart", UNIT_NAME])?;
        println!("enabled and started (you're in the {SESSION_UNIT} session)");
    } else {
        println!("enabled; it starts with your next \"Niri (virtual outputs)\" login");
    }
    Ok(())
}

/// Disables and removes the unit; `stop_now` also stops a running instance.
pub fn uninstall(stop_now: bool) -> anyhow::Result<()> {
    let path = unit_path()?;
    if !path.exists() {
        println!("{} not found; nothing to remove", path.display());
        return Ok(());
    }
    // Stopping runs the daemon's SIGTERM cleanup, which removes the virtual output.
    if stop_now {
        systemctl(&["disable", "--now", UNIT_NAME])?;
    } else {
        systemctl(&["disable", UNIT_NAME])?;
    }
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    systemctl(&["daemon-reload"])?;
    println!("stopped, disabled and removed {}", path.display());
    Ok(())
}

fn systemctl(args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("running systemctl")?;
    anyhow::ensure!(status.success(), "systemctl --user {} failed", args.join(" "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_runs_given_executable_with_session() {
        let text = unit_text("/home/u/Apps/UltraDisplay-x86_64.AppImage", &["--hidden"]);
        assert!(text.contains("ExecStart=\"/home/u/Apps/UltraDisplay-x86_64.AppImage\" --hidden\n"));
        assert!(text.contains("PartOf=niri-virtual.service\n"));
        assert!(text.contains("WantedBy=niri-virtual.service\n"));
    }
}
