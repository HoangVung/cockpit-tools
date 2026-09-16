/**
 * Codex 官方客户端临时登录：主进程「打开浏览器」拦截脚本。
 *
 * 由临时登录会话以 `NODE_OPTIONS=--require=<该文件>` 注入官方客户端主进程，
 * 只做一件事：把官方客户端准备交给系统浏览器的授权地址（auth.openai.com）
 * 改成写入采集文件，让 Cockpit 弹框可以展示与复制。
 *
 * 说明：
 * - 地址完全由官方客户端生成，这里不做任何改写、拼接或参数替换；
 * - 只拦截官方授权地址，其它链接仍按官方逻辑交给系统处理；
 * - 官方登录会话在等服务端回调，这里不打开浏览器不会中断登录；
 * - 任何异常都必须被吞掉：注入脚本抛错会导致官方客户端启动失败。
 *
 * 注入时机：`--require` 在 Electron 初始化之前执行，此时 `require('electron')`
 * 还不可用（实测报 Cannot find module 'electron'）。因此这里挂到 `Module._load`
 * 上，等官方客户端自己 require('electron') 的那一刻再接管 `shell.openExternal`，
 * 这样补丁一定早于官方发起登录跳转，也不依赖官方客户端的内部实现。
 */
'use strict';

(function () {
  var captureFile = process.env.COCKPIT_CODEX_AUTH_CAPTURE_FILE;
  if (!captureFile) {
    return;
  }

  function appendRecord(record) {
    try {
      require('node:fs').appendFileSync(
        captureFile,
        JSON.stringify(record) + '\n'
      );
    } catch (error) {
      // 采集失败不能影响官方客户端运行。
    }
  }

  // 只在 Electron 主进程生效：只有它持有 electron.shell。
  if (process.type !== 'browser') {
    return;
  }

  /**
   * 是否属于官方登录授权跳转。
   *
   * 实测官方实际交给浏览器的是两层地址之一：
   * - 桌面授权包装页：`https://chatgpt.com/codex/desktop-auth?authorize_url=<下一条>`
   * - 直连授权端点：`https://auth.openai.com/oauth/authorize?...`
   * 两个都要认，否则补丁生效了也抓不到。
   */
  function isAuthUrl(value) {
    if (typeof value !== 'string') {
      return false;
    }
    var raw = value.trim();
    if (!/^https:\/\//i.test(raw)) {
      return false;
    }
    try {
      var parsed = new URL(raw);
      var host = parsed.hostname.toLowerCase();
      if (host === 'auth.openai.com') {
        return parsed.pathname.indexOf('/oauth/authorize') === 0;
      }
      if (host === 'chatgpt.com' || host.endsWith('.chatgpt.com')) {
        return parsed.pathname === '/codex/desktop-auth';
      }
      return false;
    } catch (error) {
      return false;
    }
  }

  function isPatched(openExternal) {
    return (
      typeof openExternal === 'function' &&
      openExternal.__cockpitAuthCapture === true
    );
  }

  /** 接管 shell.openExternal：只吞掉官方授权地址，其余原样交给官方逻辑。 */
  function install(shell) {
    try {
      if (!shell || typeof shell.openExternal !== 'function') {
        appendRecord({
          kind: 'error',
          message: 'shell.openExternal is unavailable',
          reportedAt: Date.now(),
        });
        return false;
      }
      if (isPatched(shell.openExternal)) {
        return true;
      }

      var originalOpenExternal = shell.openExternal.bind(shell);
      var patched = function (url, options) {
        if (!isAuthUrl(url)) {
          return originalOpenExternal(url, options);
        }
        appendRecord({
          kind: 'url',
          url: String(url).trim(),
          capturedAt: Date.now(),
        });
        // 官方登录会话在等服务端回调，这里不打开浏览器不会中断登录。
        return Promise.resolve();
      };
      patched.__cockpitAuthCapture = true;
      shell.openExternal = patched;

      appendRecord({
        kind: 'armed',
        pid: process.pid,
        armedAt: Date.now(),
      });
      return true;
    } catch (error) {
      appendRecord({
        kind: 'error',
        message: String((error && error.message) || error),
        reportedAt: Date.now(),
      });
      return false;
    }
  }

  try {
    // 官方客户端 require('electron') 时立即接管（主进程会多次 require，重复调用无副作用）。
    var Module = require('module');
    var originalLoad = Module._load;
    Module._load = function (request) {
      var loaded = originalLoad.apply(this, arguments);
      if (request === 'electron' || request === 'electron/main') {
        install(loaded && loaded.shell);
      }
      return loaded;
    };

    // 兜底：极少数情况下 electron 已经加载过，这里补一次。
    var attempts = 0;
    var timer = setInterval(function () {
      attempts += 1;
      var patched = false;
      try {
        var loaded = require('electron');
        patched = install(loaded && loaded.shell);
      } catch (error) {
        // Electron 初始化完成之前可能仍取不到模块，继续等待。
      }
      if (patched || attempts >= 120) {
        clearInterval(timer);
      }
    }, 250);
  } catch (error) {
    appendRecord({
      kind: 'error',
      message: String((error && error.message) || error),
      reportedAt: Date.now(),
    });
  }
})();
