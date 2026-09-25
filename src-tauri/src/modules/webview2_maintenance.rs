use std::path::PathBuf;

#[cfg(target_os = "windows")]
use std::collections::HashSet;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
use std::process::Command;
#[cfg(target_os = "windows")]
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const EBWEBVIEW_DIR_IDENTIFIER: &str = "com.jlcodes.cockpit-tools";
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[cfg(target_os = "windows")]
fn resolve_ebwebview_data_dir() -> Option<PathBuf> {
    if let Some(local_appdata) = dirs::data_local_dir() {
        return Some(local_appdata.join(EBWEBVIEW_DIR_IDENTIFIER).join("EBWebView"));
    }
    if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        let trimmed = local_appdata.trim();
        if !trimmed.is_empty() {
            return Some(
                PathBuf::from(trimmed)
                    .join(EBWEBVIEW_DIR_IDENTIFIER)
                    .join("EBWebView"),
            );
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn is_cockpit_webview2_cmd(cmd_line: &str) -> bool {
    let lower = cmd_line.to_ascii_lowercase();
    (lower.contains("com.jlcodes.cockpit-tools") && lower.contains("ebwebview"))
        || lower.contains("--webview-exe-name=cockpit-tools.exe")
}

#[cfg(target_os = "windows")]
fn kill_pid_force(pid: u32) -> bool {
    if pid == 0 || pid == std::process::id() {
        return false;
    }
    let status = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    match status {
        Ok(s) => s.success(),
        Err(_) => false,
    }
}

/// On Windows, clean up any orphaned `msedgewebview2.exe` processes and stale lock files
/// before Tauri attempts to initialize WebView2.
///
/// If a previous Cockpit Tools instance crashed or exited without WebView2 shutting down,
/// the orphaned WebView2 process retains an exclusive lock on the `EBWebView` profile directory.
/// When the new instance starts, WebView2 hangs waiting on the lock for 120 seconds and then
/// fails with `0x800705B4` (ERROR_TIMEOUT), resulting in a blank white screen.
#[cfg(target_os = "windows")]
pub fn cleanup_orphaned_webview2_processes() {
    let current_pid = std::process::id();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet),
    );

    // Find all running cockpit-tools.exe processes
    let mut active_cockpit_pids = HashSet::new();
    for (pid, process) in system.processes() {
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        if name == "cockpit-tools.exe" {
            active_cockpit_pids.insert(pid.as_u32());
        }
    }
    active_cockpit_pids.insert(current_pid);

    let other_cockpit_instances_running = active_cockpit_pids
        .iter()
        .any(|&pid| pid != current_pid);

    let mut orphaned_webview_pids = Vec::new();

    for (pid, process) in system.processes() {
        let pid_u32 = pid.as_u32();
        if pid_u32 == current_pid {
            continue;
        }

        let name = process.name().to_string_lossy().to_ascii_lowercase();
        if name != "msedgewebview2.exe" {
            continue;
        }

        let cmd_joined = process
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");

        if !is_cockpit_webview2_cmd(&cmd_joined) {
            continue;
        }

        // Check parent PID: if parent is missing or dead, or not an active cockpit-tools instance,
        // it is definitely an orphan from a previous run.
        let is_orphan = match process.parent() {
            Some(parent_pid) => {
                let parent_u32 = parent_pid.as_u32();
                // At startup, current_pid hasn't spawned any webview yet.
                // If parent is dead, or parent is not among running cockpit-tools processes, it's orphaned.
                !active_cockpit_pids.contains(&parent_u32)
            }
            None => true,
        };

        // If no other cockpit-tools instance is running, ALL webviews targeting our user data dir are orphans!
        if is_orphan || !other_cockpit_instances_running {
            orphaned_webview_pids.push(pid_u32);
        }
    }

    if !orphaned_webview_pids.is_empty() {
        crate::modules::logger::log_info(&format!(
            "[WebView2Maintenance] Found {} orphaned WebView2 processes: {:?}",
            orphaned_webview_pids.len(),
            orphaned_webview_pids
        ));
        for pid in &orphaned_webview_pids {
            let killed = kill_pid_force(*pid);
            crate::modules::logger::log_info(&format!(
                "[WebView2Maintenance] Force-killed orphaned WebView2 pid={}: success={}",
                pid, killed
            ));
        }
    }

    // Only clean stale lockfiles if no other Cockpit Tools instance is alive
    if !other_cockpit_instances_running {
        if let Some(ebwebview_dir) = resolve_ebwebview_data_dir() {
            if ebwebview_dir.exists() {
                let lockfile = ebwebview_dir.join("lockfile");
                if lockfile.exists() {
                    let _ = std::fs::remove_file(&lockfile);
                    crate::modules::logger::log_info("[WebView2Maintenance] Removed stale EBWebView/lockfile");
                }
                let default_lock = ebwebview_dir.join("Default").join("LOCK");
                if default_lock.exists() {
                    let _ = std::fs::remove_file(&default_lock);
                    crate::modules::logger::log_info("[WebView2Maintenance] Removed stale EBWebView/Default/LOCK");
                }
            }
        }
    }
}

/// On Windows, cleanly terminate any WebView2 processes belonging to the current process on exit.
#[cfg(target_os = "windows")]
pub fn cleanup_current_webview2_processes() {
    let current_pid = std::process::id();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet),
    );

    let mut my_webview_pids = Vec::new();
    for (pid, process) in system.processes() {
        let pid_u32 = pid.as_u32();
        if pid_u32 == current_pid {
            continue;
        }

        let name = process.name().to_string_lossy().to_ascii_lowercase();
        if name != "msedgewebview2.exe" {
            continue;
        }

        let is_my_child = process
            .parent()
            .map(|p| p.as_u32() == current_pid)
            .unwrap_or(false);

        let cmd_joined = process
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");

        if is_my_child || is_cockpit_webview2_cmd(&cmd_joined) {
            my_webview_pids.push(pid_u32);
        }
    }

    if !my_webview_pids.is_empty() {
        crate::modules::logger::log_info(&format!(
            "[WebView2Maintenance] Cleaning up {} WebView2 processes at app exit: {:?}",
            my_webview_pids.len(),
            my_webview_pids
        ));
        for pid in my_webview_pids {
            let _ = kill_pid_force(pid);
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn cleanup_orphaned_webview2_processes() {}

#[cfg(not(target_os = "windows"))]
pub fn cleanup_current_webview2_processes() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_cockpit_webview2_cmd() {
        #[cfg(target_os = "windows")]
        {
            let sample_cmd = r#""C:\Program Files (x86)\Microsoft\EdgeWebView\Application\msedgewebview2.exe" --user-data-dir="C:\Users\ADMIN\AppData\Local\com.jlcodes.cockpit-tools\EBWebView" --webview-exe-name=cockpit-tools.exe"#;
            assert!(is_cockpit_webview2_cmd(sample_cmd));

            let unrelated_cmd = r#""C:\Program Files (x86)\Microsoft\EdgeWebView\Application\msedgewebview2.exe" --user-data-dir="C:\Users\ADMIN\AppData\Local\Microsoft\Teams\EBWebView""#;
            assert!(!is_cockpit_webview2_cmd(unrelated_cmd));
        }
    }
}
