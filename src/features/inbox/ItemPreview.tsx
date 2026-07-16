import { FileWarning, RefreshCw } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

interface ItemPreviewProps {
  originalName: string;
  previewUrl: string;
}

type PreviewVariant = "original" | "normalized";

function withVariant(previewUrl: string, variant: PreviewVariant) {
  try {
    const url = new URL(previewUrl);
    url.searchParams.set("variant", variant);
    return url.toString();
  } catch {
    return previewUrl;
  }
}

export function ItemPreview({ originalName, previewUrl }: ItemPreviewProps) {
  const hasNormalized = previewUrl.includes("variant=normalized");
  const [variant, setVariant] = useState<PreviewVariant>(
    hasNormalized ? "normalized" : "original",
  );
  const [failed, setFailed] = useState(false);
  const [revision, setRevision] = useState(0);
  const frameRef = useRef<HTMLIFrameElement>(null);
  const source = useMemo(
    () => withVariant(previewUrl, variant),
    [previewUrl, variant],
  );

  useEffect(() => {
    const frame = frameRef.current;
    if (!frame) return;
    const handleError = () => setFailed(true);
    frame.addEventListener("error", handleError);
    return () => frame.removeEventListener("error", handleError);
  }, [revision, source]);

  function chooseVariant(nextVariant: PreviewVariant) {
    setVariant(nextVariant);
    setFailed(false);
  }

  return (
    <section className="item-preview" aria-labelledby="item-preview-title">
      <header>
        <div>
          <h3 id="item-preview-title">文件预览</h3>
          <span>{originalName}</span>
        </div>
        {hasNormalized ? (
          <div className="preview-variants" aria-label="预览版本">
            <button
              type="button"
              aria-pressed={variant === "original"}
              onClick={() => chooseVariant("original")}
            >
              原件
            </button>
            <button
              type="button"
              aria-pressed={variant === "normalized"}
              onClick={() => chooseVariant("normalized")}
            >
              标准化件
            </button>
          </div>
        ) : (
          <span className="preview-original-label">原件</span>
        )}
      </header>
      <div className="item-preview-surface">
        {failed ? (
          <div className="preview-error" role="alert">
            <FileWarning size={24} strokeWidth={1.6} aria-hidden="true" />
            <strong>无法显示票据预览</strong>
            <span>文件可能已移动、损坏或暂不支持此格式。</span>
            <button
              className="button button-secondary"
              type="button"
              onClick={() => {
                setFailed(false);
                setRevision((current) => current + 1);
              }}
            >
              <RefreshCw size={15} strokeWidth={1.7} aria-hidden="true" />
              重新加载
            </button>
          </div>
        ) : (
          <iframe
            ref={frameRef}
            key={`${source}-${revision}`}
            title="票据预览"
            src={source}
          />
        )}
      </div>
    </section>
  );
}
