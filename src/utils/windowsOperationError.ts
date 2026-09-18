const STRUCTURED_PREFIX = "WINDOWS_OPERATION_ERROR:";

/**
 * 多开实例（客户端模式）与「官方登录」启动被系统拒绝时后端回传的标记：
 * 直启 WindowsApps 内的 ChatGPT.exe、PowerShell 兜底与包身份启动都被拒绝，
 * 后端为避免打开默认账号而放弃商店入口兜底。
 */
export const CODEX_MANAGED_STORE_LAUNCH_UNSAFE_PREFIX =
  "CODEX_MANAGED_STORE_LAUNCH_UNSAFE:";

export type WindowsOperationKind =
  | "launch_app"
  | "stop_process"
  | "write_file"
  | "replace_file"
  | "delete_file"
  | "start_sidecar"
  | "open_path"
  | "backup"
  | "unknown";

export type WindowsOperationErrorCode =
  | "access_denied"
  | "file_in_use"
  | "program_not_found"
  | "port_denied"
  | "codex_store_launch_blocked"
  | "operation_failed";

export interface WindowsOperationDiagnostic {
  label: string;
  value: string;
}

export interface WindowsOperationErrorDetail {
  code: WindowsOperationErrorCode;
  operation: WindowsOperationKind;
  summary: string;
  originalReason: string;
  target: string | null;
  pids: number[];
  retryable: boolean;
  canElevate: boolean;
  manualActionAvailable: boolean;
  attemptedRecoveries: string[];
  /** 后端附带的诊断字段（launch_path / registered_path 等），仅用于展示与复制。 */
  diagnostics: WindowsOperationDiagnostic[];
}

interface StructuredWindowsOperationError {
  code?: unknown;
  operation?: unknown;
  summary?: unknown;
  originalReason?: unknown;
  target?: unknown;
  pids?: unknown;
  retryable?: unknown;
  canElevate?: unknown;
  manualActionAvailable?: unknown;
  attemptedRecoveries?: unknown;
}

function currentPlatform(): string {
  if (typeof navigator === "undefined") return "";
  const nav = navigator as Navigator & { userAgentData?: { platform?: string } };
  return nav.userAgentData?.platform || navigator.platform || navigator.userAgent || "";
}

export function isWindowsRuntime(platform = currentPlatform()): boolean {
  return platform.toLowerCase().includes("win");
}

export function redactWindowsOperationError(raw: string): string {
  return raw
    .replace(
      /\b(authorization)\s*:\s*bearer\s+[A-Za-z0-9._~+\/-]+/gi,
      "$1: Bearer [REDACTED]",
    )
    .replace(
      /\b(access_token|id_token|refresh_token|api[_-]?key|password|cookie)\b(\s*[=:]\s*["']?)([^\s,"'}]+)/gi,
      "$1$2[REDACTED]",
    )
    .replace(
      /\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b/g,
      "[REDACTED_JWT]",
    )
    .replace(/\brt\.[A-Za-z0-9._~-]{20,}\b/g, "[REDACTED_REFRESH_TOKEN]");
}

function normalizePids(value: unknown): number[] {
  if (!Array.isArray(value)) return [];
  return [...new Set(value
    .map((item) => Number(item))
    .filter((item) => Number.isInteger(item) && item > 0 && item <= 0xffff_ffff))];
}

function parseStructured(raw: string): WindowsOperationErrorDetail | null {
  const marker = raw.indexOf(STRUCTURED_PREFIX);
  if (marker < 0) return null;
  try {
    const payload = JSON.parse(
      raw.slice(marker + STRUCTURED_PREFIX.length).trim(),
    ) as StructuredWindowsOperationError;
    const originalReason = redactWindowsOperationError(
      typeof payload.originalReason === "string"
        ? payload.originalReason
        : raw,
    );
    const operation = typeof payload.operation === "string"
      ? payload.operation as WindowsOperationKind
      : "unknown";
    const code = typeof payload.code === "string"
      ? payload.code as WindowsOperationErrorCode
      : classifyCode(originalReason);
    return {
      code,
      operation,
      summary:
        typeof payload.summary === "string" && payload.summary.trim()
          ? payload.summary.trim()
          : originalReason,
      originalReason,
      target:
        typeof payload.target === "string" && payload.target.trim()
          ? payload.target.trim()
          : null,
      pids: normalizePids(payload.pids),
      retryable: payload.retryable !== false,
      canElevate: payload.canElevate === true,
      manualActionAvailable: payload.manualActionAvailable === true,
      attemptedRecoveries: Array.isArray(payload.attemptedRecoveries)
        ? payload.attemptedRecoveries.map(String).filter(Boolean)
        : [],
      diagnostics: [],
    };
  } catch {
    return null;
  }
}

/** 后端在商店版启动失败时附带的 `key=value` 诊断字段。 */
const CODEX_STORE_LAUNCH_DIAGNOSTIC_KEYS = [
  "launch_path",
  "launch_path_exists",
  "registered_path",
  "path_matches_registered",
] as const;

function readDiagnosticField(payload: string, name: string): string | null {
  const match = payload.match(new RegExp(`(?:^|[;\\s])${name}=([^;]*)`));
  const value = match?.[1]?.trim();
  return value ? value : null;
}

/**
 * 解析 `CODEX_MANAGED_STORE_LAUNCH_UNSAFE:` 错误。
 *
 * 这类错误说明商店版 Codex 的三种启动方式（直启 / PowerShell / 包身份）都被系统拒绝，
 * 后端已阻止启动以免打开错误账号。前端据此给出「重新检测路径并重试」的修复入口。
 */
function parseCodexStoreLaunchBlocked(
  raw: string,
  defaults?: {
    operation?: WindowsOperationKind;
    target?: string | null;
    summary?: string;
  },
): WindowsOperationErrorDetail | null {
  const marker = raw.indexOf(CODEX_MANAGED_STORE_LAUNCH_UNSAFE_PREFIX);
  if (marker < 0) return null;

  const payload = raw.slice(marker + CODEX_MANAGED_STORE_LAUNCH_UNSAFE_PREFIX.length);
  const diagnostics: WindowsOperationDiagnostic[] = [];
  for (const key of CODEX_STORE_LAUNCH_DIAGNOSTIC_KEYS) {
    const value = readDiagnosticField(payload, key);
    if (value) diagnostics.push({ label: key, value });
  }
  const launchPath = readDiagnosticField(payload, "launch_path");

  return {
    code: "codex_store_launch_blocked",
    operation: defaults?.operation ?? "launch_app",
    summary: defaults?.summary?.trim() || raw.split(/\r?\n/, 1)[0],
    originalReason: redactWindowsOperationError(raw),
    target: launchPath ?? defaults?.target?.trim() ?? null,
    pids: [],
    retryable: true,
    canElevate: false,
    manualActionAvailable: false,
    attemptedRecoveries: [],
    diagnostics,
  };
}

function classifyCode(raw: string): WindowsOperationErrorCode {
  const lower = raw.toLowerCase();
  if (
    lower.includes("os error 5") ||
    lower.includes("permissiondenied") ||
    lower.includes("permission denied") ||
    lower.includes("access is denied") ||
    lower.includes("access denied") ||
    raw.includes("拒绝访问")
  ) {
    return "access_denied";
  }
  if (
    lower.includes("os error 32") ||
    lower.includes("sharing violation") ||
    lower.includes("being used by another process") ||
    lower.includes("file is in use") ||
    raw.includes("文件被占用") ||
    raw.includes("正在使用")
  ) {
    return "file_in_use";
  }
  if (
    lower.includes("program not found") ||
    lower.includes("the system cannot find the file specified") ||
    lower.includes("os error 2")
  ) {
    return "program_not_found";
  }
  if (lower.includes("os error 10013") || lower.includes("wsaeacces")) {
    return "port_denied";
  }
  return "operation_failed";
}

function extractPids(raw: string): number[] {
  const matches = [...raw.matchAll(/\bpids?\s*[=:]\s*([0-9,\s]+)/gi)];
  const values = matches.flatMap((match) => match[1].split(/[\s,]+/));
  return normalizePids(values);
}

function extractWindowsTarget(raw: string): string | null {
  const match = raw.match(/[A-Za-z]:\\[^\r\n)]+/);
  if (!match) return null;
  return match[0].replace(/\s+\/\s+CODEX_HOME=.*$/i, "").trim();
}

export function parseWindowsOperationError(
  error: unknown,
  defaults?: {
    operation?: WindowsOperationKind;
    target?: string | null;
    summary?: string;
    platform?: string;
  },
): WindowsOperationErrorDetail | null {
  if (!isWindowsRuntime(defaults?.platform)) return null;
  const raw = String(error ?? "").replace(/^Error:\s*/, "").trim();
  if (!raw) return null;

  const storeLaunchBlocked = parseCodexStoreLaunchBlocked(raw, defaults);
  if (storeLaunchBlocked) return storeLaunchBlocked;

  const structured = parseStructured(raw);
  if (structured) return structured;

  const code = classifyCode(raw);
  if (code === "operation_failed") return null;
  const operation = defaults?.operation ?? "unknown";
  const pids = extractPids(raw);
  return {
    code,
    operation,
    summary: defaults?.summary?.trim() || raw.split(/\r?\n/, 1)[0],
    originalReason: redactWindowsOperationError(raw),
    target: defaults?.target?.trim() || extractWindowsTarget(raw),
    pids,
    retryable: true,
    canElevate: code === "access_denied" && operation === "stop_process" && pids.length > 0,
    manualActionAvailable: operation === "stop_process",
    attemptedRecoveries: [],
    diagnostics: [],
  };
}
