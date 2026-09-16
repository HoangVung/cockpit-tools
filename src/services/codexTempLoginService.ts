import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { CodexAccount } from '../types/codex';

/**
 * 官方客户端临时登录：打开官方 Codex 客户端完成一次真实登录，
 * 读取到登录信息后自动关闭客户端并清理临时 profile。
 */
export type CodexTempLoginPhase =
  | 'preparing'
  | 'launching'
  | 'waiting-login'
  | 'importing'
  | 'closing'
  | 'cleaning'
  | 'completed'
  | 'failed'
  | 'cancelled';

export interface CodexTempLoginProgress {
  sessionId: string;
  phase: CodexTempLoginPhase;
  progress: number;
  accountId?: string | null;
  email?: string | null;
  /** completed 阶段回传的账号快照（与账号列表返回的结构一致）。 */
  account?: CodexAccount | null;
  /** 失败原因；completed 时为清理告警（清理交由后续巡检重试）。 */
  error?: string | null;
  /** 已截获的官方授权地址（官方客户端生成，原样展示）。 */
  authUrl?: string | null;
}

/**
 * 官方授权地址的截获状态：
 * - `armed`：官方主进程注入已生效，点「继续登录」不会再跳转浏览器；
 * - `captured`：已截获官方生成的授权地址（`url` 为原文）；
 * - `unavailable`：本次注入未生效，官方会照常打开浏览器登录。
 */
export type CodexTempLoginAuthUrlStatus = 'armed' | 'captured' | 'unavailable';

export interface CodexTempLoginAuthUrlEvent {
  sessionId: string;
  status: CodexTempLoginAuthUrlStatus;
  url?: string | null;
  error?: string | null;
}

export interface CodexTempLoginSession {
  sessionId: string;
  profileDir: string;
}

export interface CodexTempLoginCleanupFailure {
  path: string;
  error: string;
}

export interface CodexTempLoginCleanupReport {
  removed: string[];
  failed: CodexTempLoginCleanupFailure[];
}

export const CODEX_TEMP_LOGIN_PROGRESS_EVENT = 'codex:temp-login-progress';
export const CODEX_TEMP_LOGIN_AUTH_URL_EVENT = 'codex:temp-login-auth-url';

/** 本次登录流程中需要展示的步骤顺序。 */
export const CODEX_TEMP_LOGIN_STEPS: CodexTempLoginPhase[] = [
  'preparing',
  'launching',
  'waiting-login',
  'importing',
  'closing',
  'cleaning',
];

/**
 * 开始一次官方登录。
 *
 * `interceptAuthUrl` 为 true（默认）时，官方客户端的"打开浏览器"会被接管，
 * 官方生成的授权地址直接显示在弹框里；为 false 时完全走官方原生流程（会打开浏览器）。
 */
export async function startCodexTempLogin(
  interceptAuthUrl = true,
): Promise<CodexTempLoginSession> {
  return await invoke<CodexTempLoginSession>('start_codex_temp_login', {
    interceptAuthUrl,
  });
}

export async function cancelCodexTempLogin(sessionId: string): Promise<void> {
  await invoke('cancel_codex_temp_login', { sessionId });
}

/** 用默认浏览器打开截获到的官方授权地址（后端只允许官方域名）。 */
export async function openCodexTempLoginAuthUrl(url: string): Promise<void> {
  await invoke('open_codex_temp_login_auth_url', { url });
}

/** 手动触发一次残留清理（正常由启动巡检与定期巡检自动完成）。 */
export async function cleanupCodexTempLoginArtifacts(): Promise<CodexTempLoginCleanupReport> {
  return await invoke<CodexTempLoginCleanupReport>('cleanup_codex_temp_login_artifacts');
}

export async function listenCodexTempLoginProgress(
  handler: (payload: CodexTempLoginProgress) => void,
): Promise<UnlistenFn> {
  return await listen<CodexTempLoginProgress>(CODEX_TEMP_LOGIN_PROGRESS_EVENT, (event) => {
    handler(event.payload);
  });
}

export async function listenCodexTempLoginAuthUrl(
  handler: (payload: CodexTempLoginAuthUrlEvent) => void,
): Promise<UnlistenFn> {
  return await listen<CodexTempLoginAuthUrlEvent>(
    CODEX_TEMP_LOGIN_AUTH_URL_EVENT,
    (event) => {
      handler(event.payload);
    },
  );
}
