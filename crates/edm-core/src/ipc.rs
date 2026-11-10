//! Thin `hyprctl` wrapper (was `HyprlandIPC`). std only.

use std::process::Command;

#[derive(Clone, Copy, Debug, Default)]
pub struct HyprlandIpc;

/// `"dispatch workspace 1"` -> `["dispatch", "workspace", "1"]`. The old code passed the whole
/// string as ONE argument, which hyprctl cannot parse.
pub fn split_command(cmd: &str) -> Vec<&str> {
    cmd.split_whitespace().collect()
}

impl HyprlandIpc {
    pub fn send_command(&self, cmd: &str) -> Result<(), String> {
        self.run(&split_command(cmd)).map(|_| ())
    }

    pub fn get_status(&self) -> Result<String, String> {
        self.run(&["activewindow"])
    }

    fn run(&self, args: &[&str]) -> Result<String, String> {
        if args.is_empty() {
            return Err("empty IPC command".into());
        }
        let out = Command::new("hyprctl")
            .args(args)
            .output()
            .map_err(|e| format!("IPC command failed: {e}"))?;
        if !out.status.success() {
            return Err(format!("IPC command error: {}", String::from_utf8_lossy(&out.stderr)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_arguments() {
        assert_eq!(split_command("dispatch  workspace 1"), ["dispatch", "workspace", "1"]);
        assert!(split_command("   ").is_empty());
        assert!(HyprlandIpc.send_command("  ").is_err());
    }
}
