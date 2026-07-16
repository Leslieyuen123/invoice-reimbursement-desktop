import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { FilePlus2, Upload } from "lucide-react";
import { useEffect, useState } from "react";

interface FileDropZoneProps {
  disabled?: boolean;
  onPaths: (paths: string[]) => void;
}

function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export function FileDropZone({ disabled = false, onPaths }: FileDropZoneProps) {
  const [dragActive, setDragActive] = useState(false);

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
          if (!disabled && payload.paths.length > 0) onPaths(payload.paths);
        }
      })
      .then((stopListening) => {
        if (disposed) stopListening();
        else unlisten = stopListening;
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [disabled, onPaths]);

  async function selectFiles() {
    const selected = await open({
      multiple: true,
      directory: false,
      title: "选择票据文件",
    });
    if (Array.isArray(selected) && selected.length > 0) onPaths(selected);
  }

  return (
    <div
      className={`file-drop-zone${dragActive ? " is-dragging" : ""}`}
      aria-label="手动上传票据"
    >
      <Upload size={17} strokeWidth={1.7} aria-hidden="true" />
      <span>{dragActive ? "松开以导入票据" : "拖入票据文件"}</span>
      <button
        className="button button-secondary"
        type="button"
        disabled={disabled}
        onClick={() => void selectFiles()}
      >
        <FilePlus2 size={16} strokeWidth={1.7} aria-hidden="true" />
        选择文件
      </button>
    </div>
  );
}
