//! Codex 官方客户端临时登录（Official desktop login）。
//!
//! 复用多开实例里「空白实例」的启动方式：为一台一次性的临时 profile 打开官方 Codex
//! 客户端，由官方客户端完成真实登录，Cockpit 只负责读取登录信息并立即销毁这台临时
//! profile。临时目录固定在应用数据目录下（不注册为实例、不影响默认实例和多开实例），
//! 成功、失败、取消都会关闭客户端并清理；清理失败会在下次启动与定期巡检时重试。

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tauri_plugin_opener::OpenerExt;

use crate::models::codex::CodexAccount;
use crate::modules::{
    account, app_lifecycle, codex_account, codex_app_injection, codex_instance, logger, process,
};

/// 临时登录 profile 的根目录（应用数据目录下），目录名固定便于巡检。
const TEMP_LOGIN_ROOT_DIR_NAME: &str = "codex-temp-login";
/// 会话标记文件：用于区分「我们创建的临时 profile」和其它误入目录。
const SESSION_MARKER_FILE_NAME: &str = ".cockpit-temp-login.json";
/// 待清理清单：记录尚未完全清理的会话（含已解析的运行目录），供巡检重试。
const PENDING_CLEANUP_FILE_NAME: &str = ".pending-cleanup.json";
const PROGRESS_EVENT: &str = "codex:temp-login-progress";
/// 主进程注入脚本：拦截官方"打开浏览器"的授权跳转，把地址写到采集文件。
const AUTH_HOOK_SCRIPT_FILE_NAME: &str = ".cockpit-auth-hook.cjs";
/// 采集文件：hook 逐行追加被拦截的授权地址（JSONL）。
const AUTH_CAPTURE_FILE_NAME: &str = ".cockpit-auth-capture.jsonl";
/// hook 通过该环境变量获知采集文件位置。
const AUTH_CAPTURE_ENV_KEY: &str = "COCKPIT_CODEX_AUTH_CAPTURE_FILE";
/// 注入脚本内容：编译进二进制，运行时写入临时 profile 再通过 NODE_OPTIONS 注入。
const AUTH_HOOK_SCRIPT: &str = include_str!("codex_temp_login_auth_hook.cjs");
/// 截获到官方授权地址（或注入不可用）时回传前端的事件。
const AUTH_URL_EVENT: &str = "codex:temp-login-auth-url";
/// 注入生效等待上限：超时仍没有 hook 回报时按「本次无法截获」处理，避免静默失败。
const AUTH_ARM_TIMEOUT: Duration = Duration::from_secs(20);
/// 凭据探测间隔：只做低成本检查（auth.json / 钥匙串条目是否存在）。
const CREDENTIAL_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// 等待用户在官方客户端完成登录的最长时间。
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
/// 关闭官方客户端的最长等待时间。
const CLOSE_TIMEOUT_SECS: u64 = 20;
/// 残留清理巡检周期与启动延迟（避免与启动高峰抢资源）。
const CLEANUP_SWEEP_INTERVAL: Duration = Duration::from_secs(600);
const CLEANUP_STARTUP_DELAY: Duration = Duration::from_secs(8);

/// 主进程注入的生效状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthCaptureStatus {
    /// hook 已加载并成功接管官方客户端的「打开浏览器」调用。
    Armed,
    /// 本次无法截获授权地址（注入失败或未生效），官方会照常打开浏览器。
    Unavailable,
}

/// 当前进程内的临时登录会话。
struct SessionRuntime {
    profile_dir: PathBuf,
    cancel: Arc<AtomicBool>,
    finished: bool,
    /// 最近一次截获的官方授权地址（官方生成，原样保存，不做任何改写）。
    auth_url: Option<String>,
    /// 主进程注入状态；`None` 表示尚未收到 hook 回报。
    auth_capture_status: Option<AuthCaptureStatus>,
    /// 是否拦截官方"打开浏览器"以直接展示授权地址；关闭时完全走官方原生流程。
    intercept_auth_url: bool,
}

static SESSIONS: LazyLock<Mutex<HashMap<String, SessionRuntime>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CLEANUP_LOOP_STARTED: AtomicBool = AtomicBool::new(false);

/// 前端拿到的会话句柄。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexTempLoginSession {
    pub session_id: String,
    pub profile_dir: String,
}

/// 单条清理失败记录（保留原因，便于排查与下次重试）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexTempLoginCleanupFailure {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexTempLoginCleanupReport {
    pub removed: Vec<String>,
    pub failed: Vec<CodexTempLoginCleanupFailure>,
}

enum SessionOutcome {
    Imported(CodexAccount),
    Cancelled,
    Failed(String),
}

fn lock_sessions() -> std::sync::MutexGuard<'static, HashMap<String, SessionRuntime>> {
    SESSIONS.lock().unwrap_or_else(|error| error.into_inner())
}

/// 临时登录 profile 根目录。
pub fn temp_login_root_dir() -> Result<PathBuf, String> {
    Ok(account::get_data_dir()?.join(TEMP_LOGIN_ROOT_DIR_NAME))
}

/// 是否存在未结束的会话（同一时间只处理一个账号）。
fn has_unfinished_session() -> bool {
    lock_sessions().values().any(|session| !session.finished)
}

fn session_profile_dir(session_id: &str) -> Option<PathBuf> {
    lock_sessions()
        .get(session_id)
        .map(|session| session.profile_dir.clone())
}

fn session_cancel_flag(session_id: &str) -> Option<Arc<AtomicBool>> {
    lock_sessions()
        .get(session_id)
        .map(|session| Arc::clone(&session.cancel))
}

fn session_auth_url(session_id: &str) -> Option<String> {
    lock_sessions()
        .get(session_id)
        .and_then(|session| session.auth_url.clone())
}

fn session_intercepts_auth_url(session_id: &str) -> bool {
    lock_sessions()
        .get(session_id)
        .is_some_and(|session| session.intercept_auth_url)
}

fn session_auth_capture_status(session_id: &str) -> Option<AuthCaptureStatus> {
    lock_sessions()
        .get(session_id)
        .and_then(|session| session.auth_capture_status)
}

/// 记录最新截获的授权地址；返回 false 表示与上一次相同（前端无需重复刷新）。
fn set_session_auth_url(session_id: &str, url: &str) -> bool {
    let mut sessions = lock_sessions();
    let Some(session) = sessions.get_mut(session_id) else {
        return false;
    };
    if session.auth_url.as_deref() == Some(url) {
        return false;
    }
    session.auth_url = Some(url.to_string());
    true
}

/// 更新主进程注入状态；返回 false 表示状态未变化，避免重复提示同一问题。
fn set_session_auth_capture_status(session_id: &str, status: AuthCaptureStatus) -> bool {
    let mut sessions = lock_sessions();
    let Some(session) = sessions.get_mut(session_id) else {
        return false;
    };
    if session.auth_capture_status == Some(status) {
        return false;
    }
    session.auth_capture_status = Some(status);
    true
}

fn mark_session_finished(session_id: &str) {
    if let Some(session) = lock_sessions().get_mut(session_id) {
        session.finished = true;
    }
}

fn forget_session(session_id: &str) {
    lock_sessions().remove(session_id);
}

fn is_active_session_dir(dir: &Path) -> bool {
    lock_sessions()
        .values()
        .any(|session| !session.finished && session.profile_dir == dir)
}

/// 待清理记录：即使 profile 目录已被删除，也能继续清理它对应的运行目录与钥匙串条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingCleanupEntry {
    session_id: String,
    profile_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    app_user_data_dir: Option<String>,
}

fn pending_cleanup_path() -> Result<PathBuf, String> {
    Ok(temp_login_root_dir()?.join(PENDING_CLEANUP_FILE_NAME))
}

fn read_pending_cleanup_entries() -> Vec<PendingCleanupEntry> {
    let Ok(path) = pending_cleanup_path() else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<PendingCleanupEntry>>(&raw).unwrap_or_default()
}

fn write_pending_cleanup_entries(entries: &[PendingCleanupEntry]) {
    let Ok(path) = pending_cleanup_path() else {
        return;
    };
    if entries.is_empty() {
        let _ = fs::remove_file(path);
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(entries) {
        Ok(serialized) => {
            if let Err(error) = fs::write(&path, serialized) {
                logger::log_warn(&format!(
                    "[Codex临时登录] 写入待清理清单失败: path={}, error={}",
                    path.display(),
                    error
                ));
            }
        }
        Err(error) => logger::log_warn(&format!(
            "[Codex临时登录] 序列化待清理清单失败: {}",
            error
        )),
    }
}

fn register_pending_cleanup(entry: PendingCleanupEntry) {
    let mut entries = read_pending_cleanup_entries();
    entries.retain(|item| item.session_id != entry.session_id);
    entries.push(entry);
    write_pending_cleanup_entries(&entries);
}

fn unregister_pending_cleanup(session_id: &str) {
    let mut entries = read_pending_cleanup_entries();
    let before = entries.len();
    entries.retain(|item| item.session_id != session_id);
    if entries.len() != before {
        write_pending_cleanup_entries(&entries);
    }
}

fn emit_progress(
    app: &AppHandle,
    session_id: &str,
    phase: &str,
    progress: u8,
    account: Option<&CodexAccount>,
    error: Option<&str>,
) {
    let payload = serde_json::json!({
        "sessionId": session_id,
        "phase": phase,
        "progress": progress,
        "accountId": account.map(|item| item.id.clone()),
        "email": account.map(|item| item.email.clone()),
        // 完成后直接回传账号快照，前端无需在刷新列表后再等待列表状态同步。
        "account": account,
        "error": error,
        // 已截获的官方授权地址；前端订阅较晚时也能从进度事件里补齐。
        "authUrl": session_auth_url(session_id),
    });
    if let Err(err) = app.emit(PROGRESS_EVENT, payload) {
        logger::log_warn(&format!(
            "[Codex临时登录] 进度事件发送失败: session_id={}, phase={}, error={}",
            session_id, phase, err
        ));
    }
}

/// 回传授权地址截获状态：`armed` 已接管、`captured` 已截获地址、`unavailable` 本次不可用。
fn emit_auth_url_event(
    app: &AppHandle,
    session_id: &str,
    status: &str,
    url: Option<&str>,
    error: Option<&str>,
) {
    let payload = serde_json::json!({
        "sessionId": session_id,
        "status": status,
        "url": url,
        "error": error,
    });
    if let Err(err) = app.emit(AUTH_URL_EVENT, payload) {
        logger::log_warn(&format!(
            "[Codex临时登录] 授权地址事件发送失败: session_id={}, status={}, error={}",
            session_id, status, err
        ));
    }
}

/// 开始一次官方登录：创建空白临时 profile 并打开官方客户端。
///
/// `intercept_auth_url` 为 true 时，会给这台官方客户端注入主进程脚本，
/// 把官方"打开浏览器"的授权跳转改成直接展示地址（地址仍由官方生成）；
/// 关闭时完全走官方原生流程（打开浏览器，可用官方自带的「复制登录链接」）。
pub fn start(app: AppHandle, intercept_auth_url: bool) -> Result<CodexTempLoginSession, String> {
    if has_unfinished_session() {
        return Err("已有一个官方登录正在进行中，请先完成或取消本次登录。".to_string());
    }

    let root = temp_login_root_dir()?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let profile_dir = root.join(&session_id);
    prepare_blank_profile_dir(&profile_dir)?;
    write_session_marker(&profile_dir, &session_id)?;

    lock_sessions().insert(
        session_id.clone(),
        SessionRuntime {
            profile_dir: profile_dir.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
            finished: false,
            auth_url: None,
            auth_capture_status: None,
            intercept_auth_url,
        },
    );
    // 先登记待清理记录：即使运行过程中进程被杀，也能按记录清理运行目录与钥匙串条目。
    register_pending_cleanup(PendingCleanupEntry {
        session_id: session_id.clone(),
        profile_dir: profile_dir.to_string_lossy().to_string(),
        app_user_data_dir: app_user_data_dir_for_profile(&profile_dir)
            .ok()
            .map(|dir| dir.to_string_lossy().to_string()),
    });

    // 上次运行如果没能清理干净，这里顺手补一次（与当前会话并行，不阻塞启动）。
    tauri::async_runtime::spawn_blocking(|| {
        log_cleanup_report(cleanup_stale_sessions());
    });

    let app_for_task = app.clone();
    let session_id_for_task = session_id.clone();
    tauri::async_runtime::spawn(async move {
        run_session(app_for_task, session_id_for_task).await;
    });

    logger::log_info(&format!(
        "[Codex临时登录] 会话已开始: session_id={}, profile_dir={}",
        session_id,
        profile_dir.display()
    ));
    Ok(CodexTempLoginSession {
        session_id,
        profile_dir: profile_dir.to_string_lossy().to_string(),
    })
}

/// 取消本次官方登录：关闭官方客户端并清理临时 profile。
pub fn cancel(session_id: &str) -> Result<(), String> {
    let sessions = lock_sessions();
    let session = sessions
        .get(session_id)
        .ok_or_else(|| "官方登录会话不存在或已结束".to_string())?;
    if session.finished {
        return Ok(());
    }
    session.cancel.store(true, Ordering::SeqCst);
    logger::log_info(&format!(
        "[Codex临时登录] 收到取消请求: session_id={}",
        session_id
    ));
    Ok(())
}

/// 用默认浏览器打开本次截获到的官方授权地址。
///
/// 地址本身来自官方客户端（采集文件原样保存），这里只做域名与协议校验，
/// 不做任何改写；校验是为了避免把任意 URL 交给系统浏览器打开。
pub fn open_captured_auth_url(app: &AppHandle, url: &str) -> Result<(), String> {
    let trimmed = url.trim();
    let parsed =
        url::Url::parse(trimmed).map_err(|error| format!("授权地址格式无效: {}", error))?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    // 官方实际交给浏览器的是 chatgpt.com 的桌面授权包装页，直连时才是 auth.openai.com。
    let is_official_host = host == "openai.com"
        || host.ends_with(".openai.com")
        || host == "chatgpt.com"
        || host.ends_with(".chatgpt.com");
    if parsed.scheme() != "https" || !is_official_host {
        return Err("授权地址不是官方域名，已拒绝打开。".to_string());
    }
    app.opener()
        .open_url(trimmed, None::<String>)
        .map_err(|error| format!("打开授权地址失败: {}", error))
}

async fn run_session(app: AppHandle, session_id: String) {
    let outcome = run_session_flow(&app, &session_id).await;

    let Some(profile_dir) = session_profile_dir(&session_id) else {
        return;
    };

    // 无论成功、失败还是取消，都必须关闭官方客户端并销毁临时 profile。
    emit_progress(&app, &session_id, "closing", 76, None, None);
    let close_error = close_temp_login_client(&profile_dir).await;
    if let Some(error) = close_error.as_deref() {
        logger::log_warn(&format!(
            "[Codex临时登录] 关闭官方客户端失败（将由定期巡检重试）: session_id={}, error={}",
            session_id, error
        ));
    }

    emit_progress(&app, &session_id, "cleaning", 88, None, None);
    let profile_dir_for_cleanup = profile_dir.clone();
    let cleanup_errors = tauri::async_runtime::spawn_blocking(move || {
        cleanup_profile_artifacts(&profile_dir_for_cleanup)
    })
    .await
    .unwrap_or_else(|error| vec![format!("清理任务执行失败: {}", error)]);
    if cleanup_errors.is_empty() {
        // 清理已完成，撤掉待清理记录；失败时保留记录交给定期巡检重试。
        unregister_pending_cleanup(&session_id);
    }
    for error in &cleanup_errors {
        logger::log_warn(&format!(
            "[Codex临时登录] 临时 profile 清理未完成（将由定期巡检重试）: session_id={}, error={}",
            session_id, error
        ));
    }

    match outcome {
        SessionOutcome::Imported(account) => {
            let cleanup_warning = close_error.or_else(|| cleanup_errors.first().cloned());
            emit_progress(
                &app,
                &session_id,
                "completed",
                100,
                Some(&account),
                cleanup_warning.as_deref(),
            );
            logger::log_info(&format!(
                "[Codex临时登录] 已读取账号并清理临时 profile: session_id={}, account_id={}, email={}",
                session_id, account.id, account.email
            ));
            // 额度/资料刷新放在后台，不阻塞登录结果回传。
            let app_for_refresh = app.clone();
            tauri::async_runtime::spawn(async move {
                let _ = crate::commands::codex::refresh_imported_codex_accounts(
                    &app_for_refresh,
                    vec![account],
                )
                .await;
            });
        }
        SessionOutcome::Cancelled => {
            emit_progress(&app, &session_id, "cancelled", 100, None, None);
            logger::log_info(&format!(
                "[Codex临时登录] 已取消并清理临时 profile: session_id={}",
                session_id
            ));
        }
        SessionOutcome::Failed(error) => {
            emit_progress(&app, &session_id, "failed", 100, None, Some(error.as_str()));
            logger::log_warn(&format!(
                "[Codex临时登录] 登录失败并已清理临时 profile: session_id={}, error={}",
                session_id, error
            ));
        }
    }

    mark_session_finished(&session_id);
    forget_session(&session_id);
}

/// 在后台线程启动官方客户端，返回启动后的实际主进程 pid。
async fn launch_temp_login_client(
    profile_dir: &Path,
    intercept_auth_url: bool,
) -> Result<u32, String> {
    let profile_dir_for_launch = profile_dir.to_path_buf();
    match tauri::async_runtime::spawn_blocking(move || {
        launch_official_client(&profile_dir_for_launch, intercept_auth_url)
    })
    .await
    {
        Ok(Ok(pid)) => Ok(pid),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(format!("启动官方客户端失败: {}", error)),
    }
}

fn log_temp_login_client_started(
    session_id: &str,
    pid: u32,
    profile_dir: &Path,
    intercepting: bool,
) {
    logger::log_info(&format!(
        "[Codex临时登录] 官方客户端已启动（{}）: session_id={}, pid={}, profile_dir={}",
        if intercepting {
            "已注入授权地址拦截"
        } else {
            "官方原生流程"
        },
        session_id,
        pid,
        profile_dir.display()
    ));
}

async fn run_session_flow(app: &AppHandle, session_id: &str) -> SessionOutcome {
    let Some(profile_dir) = session_profile_dir(session_id) else {
        return SessionOutcome::Failed("官方登录会话不存在".to_string());
    };
    let Some(cancel) = session_cancel_flag(session_id) else {
        return SessionOutcome::Failed("官方登录会话不存在".to_string());
    };

    emit_progress(app, session_id, "preparing", 6, None, None);
    if let Err(error) = prepare_blank_profile_dir(&profile_dir) {
        return SessionOutcome::Failed(error);
    }
    let intercept_auth_url = session_intercepts_auth_url(session_id);
    // 注入脚本必须先于官方客户端启动落盘，官方主进程才能加载它。
    let capture_path = match intercept_auth_url {
        true => match prepare_auth_hook(&profile_dir) {
            Ok(path) => Some(path),
            Err(error) => return SessionOutcome::Failed(error),
        },
        false => None,
    };

    emit_progress(app, session_id, "launching", 14, None, None);
    let mut client_pid = match launch_temp_login_client(&profile_dir, intercept_auth_url).await {
        Ok(pid) => pid,
        Err(error) => return SessionOutcome::Failed(error),
    };
    log_temp_login_client_started(session_id, client_pid, &profile_dir, intercept_auth_url);
    emit_progress(app, session_id, "waiting-login", 24, None, None);

    let started_at = Instant::now();
    let mut launched_at = Instant::now();
    let mut capture_reader = AuthCaptureReader::default();
    let mut arm_timeout_reported = false;
    // 当前是否仍处于「注入拦截」模式：注入让官方客户端起不来时会退回官方原生流程。
    let mut intercepting = intercept_auth_url;
    // 只允许自动退回一次，避免官方客户端被反复拉起。
    let mut injection_fallback_used = false;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return SessionOutcome::Cancelled;
        }
        if app_lifecycle::is_shutdown_started() {
            return SessionOutcome::Failed("应用正在退出，本次官方登录已中断".to_string());
        }
        if started_at.elapsed() >= LOGIN_TIMEOUT {
            return SessionOutcome::Failed(
                "等待官方客户端登录超时（10 分钟），本次登录已取消。".to_string(),
            );
        }

        if let Some(capture_path) = capture_path.as_deref() {
            for record in capture_reader.poll(capture_path) {
                handle_auth_capture_record(app, session_id, record);
            }
            // 注入没有回报时按不可用处理一次，避免「点了继续登录却什么都没发生」的静默失败。
            if !arm_timeout_reported
                && session_auth_capture_status(session_id).is_none()
                && launched_at.elapsed() >= AUTH_ARM_TIMEOUT
            {
                arm_timeout_reported = true;
                mark_auth_capture_unavailable(app, session_id, "官方客户端主进程注入未生效");
            }
        }

        let client_running = process::is_pid_running(client_pid);
        if official_credentials_present(&profile_dir) {
            emit_progress(app, session_id, "importing", 56, None, None);
            match codex_account::import_from_local_at(&profile_dir) {
                Ok(account) => {
                    // 与「获取本地账号」一致：命中当前账号时同步运行态授权信息。
                    crate::commands::codex::reactivate_imported_current_if_needed(
                        std::slice::from_ref(&account),
                    )
                    .await;
                    return SessionOutcome::Imported(account);
                }
                Err(error) => {
                    // 官方客户端可能仍在写入凭据，继续等待下一轮检查。
                    logger::log_info(&format!(
                        "[Codex临时登录] 已检测到登录信息但暂不可用，继续等待: session_id={}, error={}",
                        session_id, error
                    ));
                }
            }
        }

        if !client_running {
            // 注入让官方客户端在启动阶段直接失败时（主进程加载注入脚本失败会立即退出，
            // 不会留下任何采集记录），退回官方原生流程再启动一次，保证这次仍能完成登录；
            // 只是授权地址不再显示在弹框里，由官方客户端照常打开浏览器。
            if intercepting
                && !injection_fallback_used
                && session_auth_capture_status(session_id).is_none()
            {
                injection_fallback_used = true;
                intercepting = false;
                mark_auth_capture_unavailable(
                    app,
                    session_id,
                    "官方客户端在注入后未能启动，已改用官方原生登录流程",
                );
                emit_progress(app, session_id, "launching", 14, None, None);
                match launch_temp_login_client(&profile_dir, false).await {
                    Ok(pid) => {
                        client_pid = pid;
                        launched_at = Instant::now();
                        log_temp_login_client_started(session_id, pid, &profile_dir, false);
                        emit_progress(app, session_id, "waiting-login", 24, None, None);
                        continue;
                    }
                    Err(error) => return SessionOutcome::Failed(error),
                }
            }
            return SessionOutcome::Failed(
                "官方客户端已关闭，但没有检测到可导入的登录信息。".to_string(),
            );
        }

        tokio::time::sleep(CREDENTIAL_POLL_INTERVAL).await;
    }
}

/// 以空白实例的方式准备临时 profile：目录必须为空，避免把其它登录状态带进来。
fn prepare_blank_profile_dir(profile_dir: &Path) -> Result<(), String> {
    if profile_dir.exists() {
        let has_entries = fs::read_dir(profile_dir)
            .map_err(|error| format!("读取临时配置目录失败 ({}): {}", profile_dir.display(), error))?
            .next()
            .is_some();
        if has_entries {
            fs::remove_dir_all(profile_dir).map_err(|error| {
                format!(
                    "重置临时配置目录失败 ({}): {}",
                    profile_dir.display(),
                    error
                )
            })?;
        }
    }
    fs::create_dir_all(profile_dir)
        .map_err(|error| format!("创建临时配置目录失败 ({}): {}", profile_dir.display(), error))
}

fn write_session_marker(profile_dir: &Path, session_id: &str) -> Result<(), String> {
    let marker = serde_json::json!({
        "sessionId": session_id,
        "createdAt": chrono::Utc::now().timestamp_millis(),
    });
    fs::write(
        profile_dir.join(SESSION_MARKER_FILE_NAME),
        serde_json::to_string_pretty(&marker).unwrap_or_default(),
    )
    .map_err(|error| format!("写入临时登录标记失败: {}", error))
}

/// 注入脚本与采集文件固定放在临时 profile 内，随会话一起硬删除。
fn auth_capture_paths(profile_dir: &Path) -> (PathBuf, PathBuf) {
    (
        profile_dir.join(AUTH_HOOK_SCRIPT_FILE_NAME),
        profile_dir.join(AUTH_CAPTURE_FILE_NAME),
    )
}

/// 写入主进程注入脚本并清空上一轮采集结果，返回本次采集文件路径。
fn prepare_auth_hook(profile_dir: &Path) -> Result<PathBuf, String> {
    let (hook_path, capture_path) = auth_capture_paths(profile_dir);
    fs::write(&hook_path, AUTH_HOOK_SCRIPT).map_err(|error| {
        format!(
            "写入授权地址拦截脚本失败 ({}): {}",
            hook_path.display(),
            error
        )
    })?;
    // 采集文件必须从空开始，避免把上一次会话的地址当成本次结果。
    if capture_path.exists() {
        let _ = fs::remove_file(&capture_path);
    }
    Ok(capture_path)
}

/// 拼装注入脚本的 `NODE_OPTIONS` 值。
fn build_auth_hook_node_options(hook_path: &Path) -> String {
    // Node 解析 `NODE_OPTIONS` 时会把引号内的反斜杠当转义符，Windows 路径会被吃掉分隔符
    // （`C:\Users\...` → `C:Users...`），`--require` 找不到文件后 Electron 会在启动阶段
    // 直接退出，官方客户端表现为「刚打开就关闭」。统一换成正斜杠（Windows 同样接受），
    // 并整体加引号以兼容含空格的路径。
    let normalized = hook_path.to_string_lossy().replace('\\', "/");
    format!("--require=\"{}\"", normalized)
}

/// 用独立 CODEX_HOME 与 Electron user-data-dir 打开官方客户端（与多开实例启动一致）。
///
/// `intercept_auth_url` 为 true 时注入主进程脚本：官方客户端把授权地址交给系统浏览器
/// 时改写为写入采集文件，由 `handle_auth_capture_record` 读取后回传前端展示。地址仍由
/// 官方客户端生成，我们不拼接、不改写，官方登录会话本身也不受影响（它在等服务端回调）。
/// 为 false 时不注入任何东西，官方按原生方式打开浏览器。
fn launch_official_client(profile_dir: &Path, intercept_auth_url: bool) -> Result<u32, String> {
    process::ensure_codex_launch_path_configured()?;
    let injection_plan = codex_app_injection::build_launch_args(&[], false)?;
    let profile_dir_string = profile_dir.to_string_lossy().to_string();
    if !intercept_auth_url {
        return process::start_codex_with_args(&profile_dir_string, &injection_plan.args);
    }
    let (hook_path, capture_path) = auth_capture_paths(profile_dir);
    let node_options = build_auth_hook_node_options(&hook_path);
    let extra_env = vec![
        ("NODE_OPTIONS".to_string(), node_options),
        (
            AUTH_CAPTURE_ENV_KEY.to_string(),
            capture_path.to_string_lossy().to_string(),
        ),
    ];
    process::start_codex_with_args_and_env(&profile_dir_string, &injection_plan.args, &extra_env)
}

/// 采集文件里的一行记录（由注入脚本写入）。
#[derive(Debug, Deserialize)]
struct AuthCaptureRecord {
    kind: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// 增量读取采集文件：只读取新增内容，尚未写完的半行留到下一次。
#[derive(Default)]
struct AuthCaptureReader {
    offset: u64,
    pending: String,
}

impl AuthCaptureReader {
    /// 读取本次新增的记录；文件不存在或读取失败时返回空列表。
    fn poll(&mut self, path: &Path) -> Vec<AuthCaptureRecord> {
        let Ok(mut file) = fs::File::open(path) else {
            return Vec::new();
        };
        let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        if len < self.offset {
            // 采集文件被重建过：从头读，避免丢掉本次会话的记录。
            self.offset = 0;
            self.pending.clear();
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut chunk = String::new();
        if file.read_to_string(&mut chunk).is_err() {
            return Vec::new();
        }
        if chunk.is_empty() {
            return Vec::new();
        }
        self.offset += chunk.len() as u64;
        self.pending.push_str(&chunk);

        let mut records = Vec::new();
        while let Some(index) = self.pending.find('\n') {
            let line = self.pending[..index].trim().to_string();
            self.pending.drain(..=index);
            if line.is_empty() {
                continue;
            }
            if let Ok(record) = serde_json::from_str::<AuthCaptureRecord>(&line) {
                records.push(record);
            }
        }
        records
    }
}

/// 处理一条采集记录：把官方授权地址（或注入失败原因）回传前端。
fn handle_auth_capture_record(app: &AppHandle, session_id: &str, record: AuthCaptureRecord) {
    match record.kind.as_str() {
        "armed" => {
            if set_session_auth_capture_status(session_id, AuthCaptureStatus::Armed) {
                logger::log_info(&format!(
                    "[Codex临时登录] 授权地址拦截已生效: session_id={}",
                    session_id
                ));
                emit_auth_url_event(app, session_id, "armed", None, None);
            }
        }
        "url" => {
            let Some(url) = record
                .url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                return;
            };
            set_session_auth_capture_status(session_id, AuthCaptureStatus::Armed);
            if set_session_auth_url(session_id, url) {
                // 地址含一次性 state，只回传前端，不写入日志。
                logger::log_info(&format!(
                    "[Codex临时登录] 已截获官方授权地址: session_id={}",
                    session_id
                ));
                emit_auth_url_event(app, session_id, "captured", Some(url), None);
            }
        }
        "error" => {
            mark_auth_capture_unavailable(
                app,
                session_id,
                record.message.as_deref().unwrap_or("主进程注入失败"),
            );
        }
        _ => {}
    }
}

/// 本次无法截获授权地址：只提示一次，官方会照常打开浏览器完成登录。
fn mark_auth_capture_unavailable(app: &AppHandle, session_id: &str, reason: &str) {
    if !set_session_auth_capture_status(session_id, AuthCaptureStatus::Unavailable) {
        return;
    }
    logger::log_warn(&format!(
        "[Codex临时登录] 本次未能截获官方授权地址，官方客户端会照常打开浏览器: session_id={}, reason={}",
        session_id, reason
    ));
    emit_auth_url_event(app, session_id, "unavailable", None, Some(reason));
}

/// 官方客户端是否已经把登录信息落到这个临时 profile。
///
/// 探测只使用低成本信号：`auth.json` 已出现凭据字段，或 macOS 钥匙串里已有该目录
/// 对应的条目（只查条目是否存在，不读取密钥，因此不会弹授权框）。
fn official_credentials_present(profile_dir: &Path) -> bool {
    auth_file_has_credentials(profile_dir)
        || codex_account::codex_keychain_entry_exists_for_dir(profile_dir)
}

fn auth_file_has_credentials(profile_dir: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(profile_dir.join("auth.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        // 官方客户端可能正在写入，解析失败时继续等待。
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    [
        "tokens",
        "OPENAI_API_KEY",
        "personal_access_token",
        "agent_identity",
        "agentIdentity",
    ]
    .iter()
    .any(|key| object.contains_key(*key))
}

async fn close_temp_login_client(profile_dir: &Path) -> Option<String> {
    let target = profile_dir.to_string_lossy().to_string();
    let profile_dir_for_close = profile_dir.to_path_buf();
    match tauri::async_runtime::spawn_blocking(move || {
        codex_app_injection::stop_for_profile(&profile_dir_for_close);
        process::close_codex_instances(std::slice::from_ref(&target), CLOSE_TIMEOUT_SECS)
    })
    .await
    {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(error) => Some(format!("关闭官方客户端任务执行失败: {}", error)),
    }
}

/// 销毁临时 profile 的全部痕迹：Electron 运行目录、钥匙串条目、配置目录。
///
/// 这里必须硬删除（不能走「移到回收站」），否则登录凭据会以副本形式留在本机。
fn cleanup_profile_artifacts(profile_dir: &Path) -> Vec<String> {
    let app_user_data_dir = app_user_data_dir_for_profile(profile_dir).ok();
    cleanup_profile_artifacts_with_runtime_dir(profile_dir, app_user_data_dir.as_deref())
}

/// 与 [`cleanup_profile_artifacts`] 相同，但运行目录由调用方给出。
///
/// 待清理记录里保存的是注册时解析好的运行目录：profile 目录被删除后
/// `canonicalize` 会失败，重新推导可能得到不同的路径，因此必须按记录删除。
fn cleanup_profile_artifacts_with_runtime_dir(
    profile_dir: &Path,
    app_user_data_dir: Option<&Path>,
) -> Vec<String> {
    let mut errors = Vec::new();

    if let Some(app_user_data_dir) = app_user_data_dir {
        if app_user_data_dir.exists() {
            if let Err(error) = fs::remove_dir_all(app_user_data_dir) {
                errors.push(format!(
                    "删除临时实例运行目录失败 ({}): {}",
                    app_user_data_dir.display(),
                    error
                ));
            }
        }
    }

    if let Err(error) = codex_account::delete_codex_keychain_entry_for_dir(profile_dir) {
        errors.push(error);
    }

    if profile_dir.exists() {
        if let Err(error) = fs::remove_dir_all(profile_dir) {
            errors.push(format!(
                "删除临时配置目录失败 ({}): {}",
                profile_dir.display(),
                error
            ));
        }
    }

    errors
}

fn app_user_data_dir_for_profile(profile_dir: &Path) -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        return codex_instance::get_macos_app_user_data_dir(profile_dir);
    }
    #[cfg(target_os = "windows")]
    {
        return codex_instance::get_windows_app_user_data_dir(profile_dir);
    }
    #[cfg(target_os = "linux")]
    {
        return codex_instance::get_linux_app_user_data_dir(profile_dir);
    }
    #[allow(unreachable_code)]
    {
        let _ = profile_dir;
        Err("当前平台不支持 Codex 实例运行目录".to_string())
    }
}

/// 清理所有残留的临时登录 profile（含本次进程之外的遗留目录）。
///
/// 先按 CODEX_HOME 关闭仍在使用该 profile 的官方客户端，再删除目录与钥匙串条目；
/// 关闭失败时不删除目录，避免留下半份数据，等下次巡检重试。
pub fn cleanup_stale_sessions() -> CodexTempLoginCleanupReport {
    let mut report = CodexTempLoginCleanupReport::default();
    let root = match temp_login_root_dir() {
        Ok(root) => root,
        Err(error) => {
            report.failed.push(CodexTempLoginCleanupFailure {
                path: TEMP_LOGIN_ROOT_DIR_NAME.to_string(),
                error,
            });
            return report;
        }
    };
    let Ok(entries) = fs::read_dir(&root) else {
        return report;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|name| name == PENDING_CLEANUP_FILE_NAME)
        {
            // 待清理清单属于巡检自身的状态文件，不能当作残留物删除。
            continue;
        }
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if !is_dir {
            if fs::remove_file(&path).is_ok() {
                report.removed.push(path.to_string_lossy().to_string());
            }
            continue;
        }
        if is_active_session_dir(&path) {
            continue;
        }

        let target = path.to_string_lossy().to_string();
        if let Err(error) =
            process::close_codex_instances(std::slice::from_ref(&target), CLOSE_TIMEOUT_SECS)
        {
            report.failed.push(CodexTempLoginCleanupFailure {
                path: target,
                error: format!("关闭官方客户端失败: {}", error),
            });
            continue;
        }

        let errors = cleanup_profile_artifacts(&path);
        if errors.is_empty() {
            report.removed.push(target);
        } else {
            for error in errors {
                report.failed.push(CodexTempLoginCleanupFailure {
                    path: target.clone(),
                    error,
                });
            }
        }
    }

    retry_pending_cleanup(&mut report);
    report
}

/// 按待清理记录重试：覆盖「profile 目录已删除但运行目录/钥匙串条目还在」的情况。
fn retry_pending_cleanup(report: &mut CodexTempLoginCleanupReport) {
    let mut pending = read_pending_cleanup_entries();
    if pending.is_empty() {
        return;
    }

    let mut survivors = Vec::new();
    for entry in pending.drain(..) {
        if is_active_session_id(&entry.session_id) {
            survivors.push(entry);
            continue;
        }

        let profile_dir = PathBuf::from(&entry.profile_dir);
        let app_user_data_dir = entry.app_user_data_dir.as_deref().map(PathBuf::from);
        let app_user_data_exists = app_user_data_dir
            .as_ref()
            .is_some_and(|dir| dir.exists());
        if !profile_dir.exists() && !app_user_data_exists {
            // 已经没有残留物，直接撤销记录。
            continue;
        }

        if profile_dir.exists() {
            let target = entry.profile_dir.clone();
            if let Err(error) =
                process::close_codex_instances(std::slice::from_ref(&target), CLOSE_TIMEOUT_SECS)
            {
                report.failed.push(CodexTempLoginCleanupFailure {
                    path: entry.profile_dir.clone(),
                    error: format!("关闭官方客户端失败: {}", error),
                });
                survivors.push(entry);
                continue;
            }
        }

        let mut errors = cleanup_profile_artifacts_with_runtime_dir(
            &profile_dir,
            app_user_data_dir.as_deref(),
        );
        if errors.is_empty() {
            report.removed.push(entry.profile_dir.clone());
        } else {
            for error in errors.drain(..) {
                report.failed.push(CodexTempLoginCleanupFailure {
                    path: entry.profile_dir.clone(),
                    error,
                });
            }
            survivors.push(entry);
        }
    }

    pending = survivors;
    write_pending_cleanup_entries(&pending);
}

fn is_active_session_id(session_id: &str) -> bool {
    lock_sessions()
        .get(session_id)
        .is_some_and(|session| !session.finished)
}

fn log_cleanup_report(report: CodexTempLoginCleanupReport) {
    if report.removed.is_empty() && report.failed.is_empty() {
        return;
    }
    logger::log_info(&format!(
        "[Codex临时登录] 残留巡检完成: removed={}, failed={}",
        report.removed.len(),
        report.failed.len()
    ));
    for failure in report.failed {
        logger::log_warn(&format!(
            "[Codex临时登录] 残留清理失败: path={}, error={}",
            failure.path, failure.error
        ));
    }
}

/// 启动定期巡检：清理历史遗留的临时登录 profile。
pub fn ensure_cleanup_loop_started() {
    if CLEANUP_LOOP_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    logger::log_info("[Codex临时登录] 残留清理巡检已启动");
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(CLEANUP_STARTUP_DELAY).await;
        loop {
            if app_lifecycle::is_shutdown_started() {
                return;
            }
            match tauri::async_runtime::spawn_blocking(cleanup_stale_sessions).await {
                Ok(report) => log_cleanup_report(report),
                Err(error) => logger::log_warn(&format!(
                    "[Codex临时登录] 残留巡检任务执行失败: {}",
                    error
                )),
            }
            tokio::time::sleep(CLEANUP_SWEEP_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 隔离测试数据目录与 HOME，避免测试写入真实应用数据。
    struct TestEnvGuard {
        root: PathBuf,
        previous_data_dir: Option<String>,
        previous_home: Option<String>,
    }

    impl TestEnvGuard {
        fn new(prefix: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("{}-{}", prefix, uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).expect("create test root");
            let previous_data_dir = std::env::var("COCKPIT_TOOLS_TEST_DATA_DIR").ok();
            let previous_home = std::env::var("HOME").ok();
            std::env::set_var("COCKPIT_TOOLS_TEST_DATA_DIR", &root);
            std::env::set_var("HOME", &root);
            Self {
                root,
                previous_data_dir,
                previous_home,
            }
        }
    }

    impl Drop for TestEnvGuard {
        fn drop(&mut self) {
            match self.previous_data_dir.as_ref() {
                Some(value) => std::env::set_var("COCKPIT_TOOLS_TEST_DATA_DIR", value),
                None => std::env::remove_var("COCKPIT_TOOLS_TEST_DATA_DIR"),
            }
            match self.previous_home.as_ref() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        crate::modules::test_support::env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    #[test]
    fn blank_profile_dir_is_created_and_reset_when_not_empty() {
        let _lock = lock_env();
        let env = TestEnvGuard::new("codex-temp-login-blank-profile");
        let profile_dir = env.root.join("profile");

        prepare_blank_profile_dir(&profile_dir).expect("create blank profile");
        assert!(profile_dir.is_dir());

        // 与多开实例的空白实例一致：已有内容必须在开始前清空，避免带进旧登录状态。
        fs::write(profile_dir.join("auth.json"), "{\"tokens\":{}}").expect("write stale auth");
        prepare_blank_profile_dir(&profile_dir).expect("reset blank profile");
        assert!(!profile_dir.join("auth.json").exists());
    }

    #[test]
    fn credentials_detection_requires_a_complete_credential_payload() {
        let _lock = lock_env();
        let env = TestEnvGuard::new("codex-temp-login-credentials");
        let profile_dir = env.root.join("profile");
        fs::create_dir_all(&profile_dir).expect("create profile dir");

        assert!(!official_credentials_present(&profile_dir));

        // 官方客户端写入中的半截文件不能被当成登录完成。
        fs::write(profile_dir.join("auth.json"), "{\"tokens\":").expect("write partial file");
        assert!(!official_credentials_present(&profile_dir));

        fs::write(profile_dir.join("auth.json"), "{\"auth_mode\":\"chatgpt\"}")
            .expect("write placeholder file");
        assert!(!official_credentials_present(&profile_dir));

        fs::write(
            profile_dir.join("auth.json"),
            serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": { "access_token": "token", "id_token": "id" },
            })
            .to_string(),
        )
        .expect("write credential file");
        assert!(official_credentials_present(&profile_dir));
    }

    #[test]
    fn cleanup_removes_artifacts_and_reports_no_errors() {
        let _lock = lock_env();
        let env = TestEnvGuard::new("codex-temp-login-cleanup");
        let profile_dir = env.root.join("codex-temp-login").join("session-a");
        prepare_blank_profile_dir(&profile_dir).expect("create blank profile");
        write_session_marker(&profile_dir, "session-a").expect("write marker");

        let errors = cleanup_profile_artifacts(&profile_dir);
        assert!(errors.is_empty(), "unexpected cleanup errors: {:?}", errors);
        assert!(!profile_dir.exists());
    }

    #[test]
    fn stale_sweep_removes_leftovers_but_keeps_the_active_session() {
        let _lock = lock_env();
        let env = TestEnvGuard::new("codex-temp-login-sweep");
        let root = temp_login_root_dir().expect("resolve temp login root");
        let active_dir = root.join("active-session");
        let leftover_dir = root.join("leftover-session");
        prepare_blank_profile_dir(&active_dir).expect("create active profile");
        prepare_blank_profile_dir(&leftover_dir).expect("create leftover profile");

        lock_sessions().insert(
            "active-session".to_string(),
            SessionRuntime {
                profile_dir: active_dir.clone(),
                cancel: Arc::new(AtomicBool::new(false)),
                finished: false,
                auth_url: None,
                auth_capture_status: None,
                intercept_auth_url: true,
            },
        );

        let report = cleanup_stale_sessions();
        lock_sessions().remove("active-session");

        assert!(!leftover_dir.exists(), "leftover session should be removed");
        assert!(active_dir.exists(), "active session must not be removed");
        assert!(
            report
                .removed
                .iter()
                .any(|path| path.ends_with("leftover-session")),
            "cleanup report should list the removed directory: {:?}",
            report.removed
        );
    }

    #[test]
    fn pending_cleanup_retries_orphaned_runtime_dir_after_profile_was_removed() {
        let _lock = lock_env();
        let env = TestEnvGuard::new("codex-temp-login-pending");
        let root = temp_login_root_dir().expect("resolve temp login root");
        let profile_dir = root.join("partial-session");
        prepare_blank_profile_dir(&profile_dir).expect("create profile");

        // 模拟「profile 目录删除成功、运行目录删除失败」的半清理状态。
        let runtime_dir = env.root.join("instances").join("codex-app-data").join("hash");
        fs::create_dir_all(&runtime_dir).expect("create runtime dir");
        register_pending_cleanup(PendingCleanupEntry {
            session_id: "partial-session".to_string(),
            profile_dir: profile_dir.to_string_lossy().to_string(),
            app_user_data_dir: Some(runtime_dir.to_string_lossy().to_string()),
        });
        fs::remove_dir_all(&profile_dir).expect("remove profile dir");

        cleanup_stale_sessions();

        assert!(
            !runtime_dir.exists(),
            "orphaned runtime dir should be removed by the sweep"
        );
        assert!(
            read_pending_cleanup_entries().is_empty(),
            "pending entry should be cleared once cleanup succeeds"
        );
    }

    #[test]
    fn auth_hook_node_options_normalizes_windows_path_separators() {
        // NODE_OPTIONS 解析会吃掉引号内的反斜杠，Windows 路径必须换成正斜杠，
        // 否则 `--require` 找不到注入脚本，Electron 主进程直接在启动阶段退出。
        let options = build_auth_hook_node_options(Path::new(
            r"C:\Users\some user\.antigravity_cockpit\codex-temp-login\abc\.cockpit-auth-hook.cjs",
        ));
        assert_eq!(
            options,
            "--require=\"C:/Users/some user/.antigravity_cockpit/codex-temp-login/abc/.cockpit-auth-hook.cjs\""
        );
        // 路径含空格时仍然要有引号，Shell / Node 才会当成一个路径。
        assert!(options.starts_with("--require=\""));
        assert!(options.ends_with('"'));
        assert!(!options.contains('\\'));
    }

    #[test]
    fn auth_capture_reader_reads_appended_records_once_complete() {
        let _lock = lock_env();
        let env = TestEnvGuard::new("codex-temp-login-capture");
        let profile_dir = env.root.join("profile");
        // 真实流程里 `prepare_blank_profile_dir` 先建目录；这里保持一致。
        prepare_blank_profile_dir(&profile_dir).expect("create blank profile");
        let capture_path = prepare_auth_hook(&profile_dir).expect("prepare auth hook");

        // 注入脚本已落盘，采集文件此时还不存在。
        assert!(profile_dir.join(AUTH_HOOK_SCRIPT_FILE_NAME).exists());
        assert!(AUTH_HOOK_SCRIPT.contains("shell.openExternal"));

        let mut reader = AuthCaptureReader::default();
        assert!(reader.poll(&capture_path).is_empty());

        fs::write(
            &capture_path,
            "{\"kind\":\"armed\",\"pid\":1}\n{\"kind\":\"url\",\"url\":\"https://auth.openai.com/oauth/authorize?state=1\"}\n",
        )
        .expect("write capture records");
        let records = reader.poll(&capture_path);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, "armed");
        assert_eq!(
            records[1].url.as_deref(),
            Some("https://auth.openai.com/oauth/authorize?state=1")
        );

        // 没有新增内容时不能重复回传同一条记录。
        assert!(reader.poll(&capture_path).is_empty());

        // 尚未写完的半行要等到换行后再解析。
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&capture_path)
            .expect("open capture file");
        std::io::Write::write_all(
            &mut file,
            b"{\"kind\":\"url\",\"url\":\"https://auth.openai.com/oauth/authorize?state=2\"}",
        )
        .expect("append partial record");
        assert!(reader.poll(&capture_path).is_empty());
        std::io::Write::write_all(&mut file, b"\n").expect("finish record");
        let records = reader.poll(&capture_path);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].url.as_deref(),
            Some("https://auth.openai.com/oauth/authorize?state=2")
        );
    }

    #[test]
    fn auth_capture_state_only_reports_new_values() {
        let _lock = lock_env();
        let session_id = format!("auth-capture-{}", uuid::Uuid::new_v4());
        lock_sessions().insert(
            session_id.clone(),
            SessionRuntime {
                profile_dir: PathBuf::from("/tmp/unused"),
                cancel: Arc::new(AtomicBool::new(false)),
                // 标记为已结束，避免与残留巡检测试互相影响。
                finished: true,
                auth_url: None,
                auth_capture_status: None,
                intercept_auth_url: true,
            },
        );

        assert!(set_session_auth_capture_status(
            &session_id,
            AuthCaptureStatus::Armed
        ));
        assert!(!set_session_auth_capture_status(
            &session_id,
            AuthCaptureStatus::Armed
        ));
        assert!(set_session_auth_url(
            &session_id,
            "https://auth.openai.com/oauth/authorize?state=1"
        ));
        assert!(!set_session_auth_url(
            &session_id,
            "https://auth.openai.com/oauth/authorize?state=1"
        ));
        assert!(set_session_auth_url(
            &session_id,
            "https://auth.openai.com/oauth/authorize?state=2"
        ));
        assert_eq!(
            session_auth_url(&session_id).as_deref(),
            Some("https://auth.openai.com/oauth/authorize?state=2")
        );
        assert!(set_session_auth_capture_status(
            &session_id,
            AuthCaptureStatus::Unavailable
        ));

        forget_session(&session_id);
    }
}
