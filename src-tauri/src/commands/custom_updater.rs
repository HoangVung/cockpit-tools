use crate::modules::logger;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomSyncStepProgress {
    pub step: usize,
    pub total: usize,
    pub title: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomSyncReport {
    pub success: bool,
    pub message: String,
    pub current_step: usize,
    pub total_steps: usize,
    pub logs: String,
}

fn resolve_repo_path(repo_path: Option<String>) -> Result<PathBuf, String> {
    if let Some(ref p) = repo_path {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            if path.join(".git").exists() {
                return Ok(path);
            }
        }
    }

    // Đường dẫn mặc định của dự án
    let default_path = PathBuf::from(r"D:\0-vung-apps\cockpit-tools");
    if default_path.join(".git").exists() {
        return Ok(default_path);
    }

    // Kiểm tra thư mục làm việc hiện tại
    if let Ok(cwd) = std::env::current_dir() {
        if cwd.join(".git").exists() {
            return Ok(cwd);
        }
        if let Some(parent) = cwd.parent() {
            if parent.join(".git").exists() {
                return Ok(parent.to_path_buf());
            }
        }
    }

    Err("Không tìm thấy thư mục git repo của Cockpit Tools. Vui lòng kiểm tra đường dẫn dự án.".to_string())
}

fn run_step(repo_dir: &Path, cmd: &str) -> Result<String, String> {
    #[cfg(windows)]
    let output = {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("powershell.exe");
        command
            .current_dir(repo_dir)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                cmd,
            ])
            .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        command.output()
    };

    #[cfg(not(windows))]
    let output = {
        let mut command = std::process::Command::new("sh");
        command.current_dir(repo_dir).args(["-c", cmd]);
        command.output()
    };

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let combined = format!("{}\n{}", stdout.trim(), stderr.trim())
                .trim()
                .to_string();

            if out.status.success() {
                Ok(combined)
            } else {
                Err(format!(
                    "Lệnh '{}' thất bại (mã thoát: {:?}):\n{}",
                    cmd,
                    out.status.code(),
                    combined
                ))
            }
        }
        Err(err) => Err(format!("Không thể khởi chạy '{}': {}", cmd, err)),
    }
}

/// Chạy quy trình đồng bộ Git và kích hoạt GitHub Actions build bản tùy biến
#[tauri::command]
pub async fn sync_and_trigger_custom_build(
    app: AppHandle,
    repo_path: Option<String>,
) -> Result<CustomSyncReport, String> {
    logger::log_info("[CustomUpdater] Bắt đầu quy trình đồng bộ repo tùy biến...");

    let repo_dir = match resolve_repo_path(repo_path) {
        Ok(path) => path,
        Err(err) => {
            logger::log_error(&format!("[CustomUpdater] Lỗi đường dẫn: {}", err));
            return Ok(CustomSyncReport {
                success: false,
                message: err,
                current_step: 0,
                total_steps: 5,
                logs: String::new(),
            });
        }
    };

    logger::log_info(&format!(
        "[CustomUpdater] Sử dụng thư mục repo: {:?}",
        repo_dir
    ));

    let steps: [(&str, &str); 5] = [
        (
            "git fetch upstream main:main",
            "1/5: Lấy cập nhật mới từ repo gốc (upstream)",
        ),
        (
            "git push origin main",
            "2/5: Đồng bộ nhánh main lên fork GitHub",
        ),
        (
            "git merge main -m \"merge: sync upstream main into my-custom\"",
            "3/5: Gộp cập nhật main vào nhánh my-custom",
        ),
        (
            "npm test",
            "4/5: Chạy bộ kiểm thử regression (npm test)",
        ),
        (
            "git push origin my-custom",
            "5/5: Đẩy lên GitHub để kích hoạt build matrix",
        ),
    ];

    let total_steps = steps.len();
    let mut all_logs = Vec::new();

    for (index, (cmd, title)) in steps.iter().enumerate() {
        let step_num = index + 1;
        logger::log_info(&format!(
            "[CustomUpdater] Bước {}/{}: {} (Lệnh: {})",
            step_num, total_steps, title, cmd
        ));

        let _ = app.emit(
            "custom-sync-progress",
            CustomSyncStepProgress {
                step: step_num,
                total: total_steps,
                title: title.to_string(),
                status: "running".to_string(),
                detail: String::new(),
            },
        );

        match run_step(&repo_dir, cmd) {
            Ok(output) => {
                all_logs.push(format!("=== Bước {}/{}: {} ===\n{}\n", step_num, total_steps, title, output));
                let _ = app.emit(
                    "custom-sync-progress",
                    CustomSyncStepProgress {
                        step: step_num,
                        total: total_steps,
                        title: title.to_string(),
                        status: "success".to_string(),
                        detail: output,
                    },
                );
            }
            Err(err) => {
                logger::log_error(&format!(
                    "[CustomUpdater] Thất bại ở bước {}/{}: {}",
                    step_num, total_steps, err
                ));
                all_logs.push(format!("=== THẤT BẠI Bước {}/{}: {} ===\n{}\n", step_num, total_steps, title, err));

                let _ = app.emit(
                    "custom-sync-progress",
                    CustomSyncStepProgress {
                        step: step_num,
                        total: total_steps,
                        title: title.to_string(),
                        status: "failed".to_string(),
                        detail: err.clone(),
                    },
                );

                return Ok(CustomSyncReport {
                    success: false,
                    message: format!("Thất bại ở bước {}: {}", step_num, title),
                    current_step: step_num,
                    total_steps,
                    logs: all_logs.join("\n"),
                });
            }
        }
    }

    logger::log_info("[CustomUpdater] Hoàn thành toàn bộ quy trình đồng bộ thành công!");

    Ok(CustomSyncReport {
        success: true,
        message: "Đồng bộ và kích hoạt build thành công trên GitHub Actions!".to_string(),
        current_step: total_steps,
        total_steps,
        logs: all_logs.join("\n"),
    })
}
