import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { FilePlus2, Upload, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";

interface FileDropZoneProps {
  disabled?: boolean;
  onPaths: (paths: string[]) => void;
}

function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export function FileDropZone({ disabled = false, onPaths }: FileDropZoneProps) {
  const [dragActive, setDragActive] = useState(false);
  const [error, setError] = useState<"dialog" | "listener" | null>(null);
  const disabledRef = useRef(disabled);
  const onPathsRef = useRef(onPaths);
  const browserInputRef = useRef<HTMLInputElement>(null);
  const browserInputEnabled =
    import.meta.env.DEV && import.meta.env.VITE_BROWSER_COMMAND_BRIDGE === "1";

  disabledRef.current = disabled;
  onPathsRef.current = onPaths;

  useEffect(() => {
    if (!isTauriRuntime()) return;

    let disposed = false;
    let unlisten: (() => void) | undefined;
    void getCurrentWebview()
      .onDragDropEvent(({ payload }) => {
        if (payload.type === "enter" || payload.type === "over") {
          setDragActive(true);
        } else if (payload.type === "leave") {
          setDragActive(false);
        } else if (payload.type === "drop") {
          setDragActive(false);
          if (!disabledRef.current && payload.paths.length > 0) {
            onPathsRef.current(payload.paths);
          }
        }
      })
      .then((stopListening) => {
        if (disposed) stopListening();
        else unlisten = stopListening;
      })
      .catch(() => {
        if (!disposed) setError("listener");
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  async function selectFiles() {
    setError(null);
    if (browserInputEnabled) {
      browserInputRef.current?.click();
      return;
    }
    try {
      const selected = await open({
        multiple: true,
        directory: false,
        title: "选择票据文件",
      });
      if (Array.isArray(selected) && selected.length > 0) {
        onPathsRef.current(selected);
      }
    } catch {
      setError("dialog");
    }
  }

  return (
    <div
      className={`file-drop-zone${dragActive ? " is-dragging" : ""}`}
      aria-label="手动上传票据"
    >
      {error ? (
        <>
          <div role="alert">
            {error === "dialog"
              ? "无法打开文件选择器，请重试"
              : "拖放导入暂不可用，请使用选择文件"}
          </div>
          <button
            className="icon-button"
            type="button"
            onClick={() => setError(null)}
            aria-label="清除上传错误"
            title="清除错误"
          >
            <X size={16} strokeWidth={1.8} aria-hidden="true" />
          </button>
        </>
      ) : (
        <>
          <Upload size={17} strokeWidth={1.7} aria-hidden="true" />
          <span>{dragActive ? "松开以导入票据" : "拖入票据文件"}</span>
        </>
      )}
      <button
        className="button button-secondary"
        type="button"
        disabled={disabled}
        onClick={() => void selectFiles()}
      >
        <FilePlus2 size={16} strokeWidth={1.7} aria-hidden="true" />
        选择文件
      </button>
      {browserInputEnabled ? (
        <input
          ref={browserInputRef}
          data-testid="browser-file-input"
          type="file"
          multiple
          hidden
          tabIndex={-1}
          aria-hidden="true"
          disabled={disabled}
          onChange={(event) => {
            const paths = Array.from(event.currentTarget.files ?? [], (file) => file.name);
            if (paths.length > 0) onPathsRef.current(paths);
            event.currentTarget.value = "";
          }}
        />
      ) : null}
    </div>
  );
}
