import { useCallback, useEffect, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import * as codexTempLoginService from "../services/codexTempLoginService";
import type { CodexTempLoginPhase } from "../services/codexTempLoginService";
import type { CodexAccount } from "../types/codex";
import { emitAccountsChanged } from "../utils/accountSyncEvents";
import { parseWindowsOperationError } from "../utils/windowsOperationError";
import type { useCodexAccountsBaseController } from "./useCodexAccountsBaseController";
import type { useCodexAccountsOAuthController } from "./useCodexAccountsOAuthController";

type CodexTempLoginControllerContext = Pick<
  ReturnType<typeof useCodexAccountsBaseController> &
    ReturnType<typeof useCodexAccountsOAuthController>,
  | "assignCodexAccountsToTargetGroup"
  | "closeAddModal"
  | "codexAccountsRef"
  | "fetchAccounts"
  | "maskAccountText"
  | "page"
  | "showAddModal"
  | "syncImportedAccountsToApiService"
  | "syncImportedToApiService"
  | "t"
>;

/**
 * 「官方登录」业务域：打开官方客户端在一个一次性空白 profile 里完成登录，
 * 后端读取登录信息后立即关闭客户端并清理临时 profile。
 */
export function useCodexTempLoginController(
  context: CodexTempLoginControllerContext,
) {
  const {
    assignCodexAccountsToTargetGroup,
    closeAddModal,
    codexAccountsRef,
    fetchAccounts,
    maskAccountText,
    page,
    showAddModal,
    syncImportedAccountsToApiService,
    syncImportedToApiService,
    t,
  } = context;

  const [tempLoginPhase, setTempLoginPhase] =
    useState<CodexTempLoginPhase | null>(null);
  const [tempLoginRunning, setTempLoginRunning] = useState(false);
  const [tempLoginCancelling, setTempLoginCancelling] = useState(false);
  const [tempLoginError, setTempLoginError] = useState<string | null>(null);
  const [tempLoginNotice, setTempLoginNotice] = useState<string | null>(null);
  // 官方客户端生成的授权地址（原样展示，可复制到本机任意浏览器完成登录）。
  const [tempLoginAuthUrl, setTempLoginAuthUrl] = useState<string | null>(null);
  const [tempLoginAuthUrlCopied, setTempLoginAuthUrlCopied] = useState(false);
  // 主进程注入未生效时才提示：本次仍由官方客户端照常打开浏览器登录。
  const [tempLoginAuthUrlUnavailable, setTempLoginAuthUrlUnavailable] =
    useState(false);
  // 是否接管官方"打开浏览器"以直接展示授权地址；默认开，关闭即完全走官方原生流程。
  const [tempLoginInterceptAuthUrl, setTempLoginInterceptAuthUrl] =
    useState(true);

  const sessionIdRef = useRef<string | null>(null);
  const syncToApiServiceRef = useRef(syncImportedToApiService);
  syncToApiServiceRef.current = syncImportedToApiService;

  const phaseMessage = useCallback(
    (phase: CodexTempLoginPhase): string => {
      switch (phase) {
        case "preparing":
          return t("codex.tempLogin.phase.prepare", "正在准备临时配置…");
        case "launching":
          return t("codex.tempLogin.phase.launch", "正在打开官方客户端…");
        case "waiting-login":
          return t(
            "codex.tempLogin.phase.wait",
            "请在官方客户端完成登录…",
          );
        case "importing":
          return t("codex.tempLogin.phase.import", "正在读取登录信息…");
        case "closing":
          return t("codex.tempLogin.phase.close", "正在关闭官方客户端…");
        case "cleaning":
          return t("codex.tempLogin.phase.clean", "正在清理临时配置…");
        default:
          return "";
      }
    },
    [t],
  );

  const resetTempLoginState = useCallback(() => {
    sessionIdRef.current = null;
    setTempLoginRunning(false);
    setTempLoginCancelling(false);
    setTempLoginPhase(null);
    setTempLoginAuthUrl(null);
    setTempLoginAuthUrlCopied(false);
    setTempLoginAuthUrlUnavailable(false);
  }, []);

  /**
   * 商店版 Codex 无法启动（直启 / PowerShell / 包身份都被系统拒绝）时，后端回传的是
   * 内部错误串，直接展示对用户不可读，这里换成可操作的说明并保留原始原因。
   */
  const describeTempLoginFailure = useCallback(
    (message: string): string => {
      const parsed = parseWindowsOperationError(message, {
        operation: "launch_app",
      });
      if (!parsed || parsed.code !== "codex_store_launch_blocked") {
        return message;
      }
      return `${t(
        "common.windowsOperation.storeLaunchBlockedDescription",
        "Windows 拒绝了商店版 Codex 客户端的全部启动方式（直接启动、PowerShell 启动、包身份启动），已阻止启动以免打开错误的账号。多数情况下需要修复或重新安装商店版 Codex（可在 Microsoft Store 检查更新并重新安装）；也可「重新检测路径并重试」；仍失败时请把该实例的启动方式改为 CLI。",
      )}\n${parsed.originalReason}`;
    },
    [t],
  );

  const reportTempLoginFailure = useCallback(
    (message: string) => {
      setTempLoginError(message);
      page.setAddStatus("error");
      page.setAddMessage(
        t("codex.tempLogin.failed", "官方登录失败：{{error}}").replace(
          "{{error}}",
          describeTempLoginFailure(message),
        ),
      );
    },
    [describeTempLoginFailure, page, t],
  );

  /** 复制官方客户端生成的授权地址（原样复制，不做任何改写）。 */
  const handleCopyCodexTempLoginAuthUrl = useCallback(async () => {
    if (!tempLoginAuthUrl) return;
    try {
      await navigator.clipboard.writeText(tempLoginAuthUrl);
      setTempLoginAuthUrlCopied(true);
      window.setTimeout(() => setTempLoginAuthUrlCopied(false), 2000);
    } catch (error) {
      console.warn("[Codex Temp Login] 复制授权地址失败:", error);
      setTempLoginNotice(
        t(
          "common.shared.export.copyFailed",
          "复制失败，请手动复制",
        ),
      );
    }
  }, [t, tempLoginAuthUrl]);

  /** 用系统默认浏览器打开该地址（后端只允许官方域名）。 */
  const handleOpenCodexTempLoginAuthUrl = useCallback(async () => {
    if (!tempLoginAuthUrl) return;
    try {
      await codexTempLoginService.openCodexTempLoginAuthUrl(tempLoginAuthUrl);
    } catch (error) {
      setTempLoginNotice(
        t(
          "codex.tempLogin.authUrl.openFailed",
          "打开授权地址失败：{{error}}",
        ).replace("{{error}}", String(error).replace(/^Error:\s*/, "")),
      );
    }
  }, [t, tempLoginAuthUrl]);

  const handleTempLoginCompleted = useCallback(
    async (account: CodexAccount | null, accountId: string | null) => {
      const email = account?.email ?? null;
      page.setAddStatus("success");
      page.setAddMessage(
        t("codex.tempLogin.success", "已获取账号：{{email}}").replace(
          "{{email}}",
          maskAccountText(email ?? ""),
        ),
      );
      try {
        await fetchAccounts();
        // 事件自带账号快照；缺失时（旧事件格式）再回退到账号列表。
        const imported =
          account ??
          (accountId
            ? codexAccountsRef.current.find((item) => item.id === accountId) ?? null
            : null);
        if (imported) {
          await assignCodexAccountsToTargetGroup([imported]);
        }
        await emitAccountsChanged({ platformId: "codex", reason: "import" });
        if (imported && syncToApiServiceRef.current) {
          try {
            await syncImportedAccountsToApiService([imported.id]);
          } catch (error) {
            page.setAddStatus("error");
            page.setAddMessage(
              t(
                "codex.importApiService.syncFailed",
                "账号已导入，但加入 API 服务失败：{{error}}",
              ).replace("{{error}}", String(error).replace(/^Error:\s*/, "")),
            );
            return;
          }
        }
      } catch (error) {
        // 账号已经在后端落盘，读取列表失败只影响当前界面刷新。
        console.warn("[Codex Temp Login] 刷新账号列表失败:", error);
      }
      setTimeout(() => {
        closeAddModal();
      }, 1200);
    },
    [
      assignCodexAccountsToTargetGroup,
      closeAddModal,
      codexAccountsRef,
      fetchAccounts,
      maskAccountText,
      page,
      syncImportedAccountsToApiService,
      t,
    ],
  );

  useEffect(() => {
    let disposed = false;
    let unlisten: UnlistenFn | null = null;

    void codexTempLoginService
      .listenCodexTempLoginProgress((payload) => {
        if (disposed) return;
        if (sessionIdRef.current && payload.sessionId !== sessionIdRef.current) {
          return;
        }

        if (payload.phase === "completed") {
          resetTempLoginState();
          setTempLoginError(null);
          setTempLoginNotice(null);
          void handleTempLoginCompleted(
            payload.account ?? null,
            payload.accountId ?? null,
          );
          return;
        }

        if (payload.phase === "failed") {
          resetTempLoginState();
          reportTempLoginFailure(
            payload.error?.trim() ||
              t("codex.tempLogin.failed", "官方登录失败：{{error}}").replace(
                "{{error}}",
                "-",
              ),
          );
          return;
        }

        if (payload.phase === "cancelled") {
          resetTempLoginState();
          setTempLoginError(null);
          setTempLoginNotice(
            t("codex.tempLogin.cancelled", "已取消官方登录，临时配置已清理。"),
          );
          page.setAddStatus("idle");
          page.setAddMessage("");
          return;
        }

        // 订阅较晚或事件顺序变化时，从进度事件里补齐已截获的地址。
        if (payload.authUrl) {
          setTempLoginAuthUrl(payload.authUrl);
        }
        setTempLoginPhase(payload.phase);
        page.setAddStatus("loading");
        page.setAddMessage(phaseMessage(payload.phase));
      })
      .then((dispose) => {
        if (disposed) {
          dispose();
          return;
        }
        unlisten = dispose;
      })
      .catch((error) => {
        console.warn("[Codex Temp Login] 订阅登录进度失败:", error);
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [handleTempLoginCompleted, page, phaseMessage, reportTempLoginFailure, resetTempLoginState, t]);

  /**
   * 订阅「官方授权地址」事件：官方客户端生成的地址被截获后立即展示，
   * 注入未生效时给出降级提示（官方会照常打开浏览器登录）。
   */
  useEffect(() => {
    let disposed = false;
    let unlisten: UnlistenFn | null = null;

    void codexTempLoginService
      .listenCodexTempLoginAuthUrl((payload) => {
        if (disposed) return;
        if (sessionIdRef.current && payload.sessionId !== sessionIdRef.current) {
          return;
        }
        if (payload.status === "captured") {
          const url = payload.url?.trim();
          if (!url) return;
          setTempLoginAuthUrl(url);
          setTempLoginAuthUrlCopied(false);
          setTempLoginAuthUrlUnavailable(false);
          return;
        }
        if (payload.status === "unavailable") {
          setTempLoginAuthUrlUnavailable(true);
        }
      })
      .then((dispose) => {
        if (disposed) {
          dispose();
          return;
        }
        unlisten = dispose;
      })
      .catch((error) => {
        console.warn("[Codex Temp Login] 订阅授权地址失败:", error);
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  /** 添加账号弹框关闭时，终止仍在进行的官方登录，避免留下无人接管的临时 profile。 */
  useEffect(() => {
    if (showAddModal || !sessionIdRef.current) return;
    const sessionId = sessionIdRef.current;
    resetTempLoginState();
    void codexTempLoginService.cancelCodexTempLogin(sessionId).catch(() => {});
  }, [resetTempLoginState, showAddModal]);

  useEffect(
    () => () => {
      const sessionId = sessionIdRef.current;
      if (!sessionId) return;
      void codexTempLoginService.cancelCodexTempLogin(sessionId).catch(() => {});
    },
    [],
  );

  const handleStartCodexTempLogin = useCallback(async () => {
    if (sessionIdRef.current) return;
    setTempLoginError(null);
    setTempLoginNotice(null);
    setTempLoginAuthUrl(null);
    setTempLoginAuthUrlCopied(false);
    setTempLoginAuthUrlUnavailable(false);
    setTempLoginPhase("preparing");
    setTempLoginRunning(true);
    page.setAddStatus("loading");
    page.setAddMessage(phaseMessage("preparing"));
    try {
      const session = await codexTempLoginService.startCodexTempLogin(
        tempLoginInterceptAuthUrl,
      );
      sessionIdRef.current = session.sessionId;
    } catch (error) {
      resetTempLoginState();
      reportTempLoginFailure(String(error).replace(/^Error:\s*/, ""));
    }
  }, [
    page,
    phaseMessage,
    reportTempLoginFailure,
    resetTempLoginState,
    tempLoginInterceptAuthUrl,
  ]);

  const handleCancelCodexTempLogin = useCallback(async () => {
    const sessionId = sessionIdRef.current;
    if (!sessionId || tempLoginCancelling) return;
    setTempLoginCancelling(true);
    page.setAddStatus("loading");
    page.setAddMessage(
      t("codex.tempLogin.cancelling", "正在取消官方登录…"),
    );
    try {
      await codexTempLoginService.cancelCodexTempLogin(sessionId);
    } catch (error) {
      setTempLoginCancelling(false);
      reportTempLoginFailure(String(error).replace(/^Error:\s*/, ""));
    }
  }, [page, reportTempLoginFailure, t, tempLoginCancelling]);

  return {
    handleCancelCodexTempLogin,
    handleCopyCodexTempLoginAuthUrl,
    handleOpenCodexTempLoginAuthUrl,
    handleStartCodexTempLogin,
    tempLoginAuthUrl,
    tempLoginAuthUrlCopied,
    tempLoginAuthUrlUnavailable,
    tempLoginInterceptAuthUrl,
    tempLoginCancelling,
    tempLoginError,
    tempLoginNotice,
    tempLoginPhase,
    tempLoginPhaseMessage: phaseMessage,
    tempLoginRunning,
    setTempLoginInterceptAuthUrl,
  };
}
