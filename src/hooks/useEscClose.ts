import { useEffect, useRef } from 'react';

/** ESC-to-close for modals. LIFO cleanup order handles stacked modals correctly. */
export function useEscClose(isOpen: boolean, onClose: () => void) {
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    if (!isOpen) return;
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        onCloseRef.current();
      }
    };
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [isOpen]);
}

/**
 * ESC 关闭「最上层」弹框。
 *
 * 同一个页面上的 `useEscClose` 监听的是同级 window 事件，弹框层叠时按下 ESC 会把
 * 上下两层一起关掉（例如添加账号弹框里再打开一个选择弹框）。这里改在捕获阶段拦截并
 * 停止传播，保证只有最上层弹框响应 ESC，其余层的处理逻辑不会被触发。
 */
export function useEscCloseTopmost(isOpen: boolean, onClose: () => void) {
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    if (!isOpen) return;
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        event.stopImmediatePropagation();
        onCloseRef.current();
      }
    };
    window.addEventListener('keydown', handleKeyDown, true);
    return () => window.removeEventListener('keydown', handleKeyDown, true);
  }, [isOpen]);
}
