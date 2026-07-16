import { FileWarning, RefreshCw } from "lucide-react";
import { useEffect, useMemo, useState } from "react";

interface ItemPreviewProps {
  originalName: string;
  previewUrl: string;
  loadAvailability?: PreviewAvailabilityLoader;
}

type PreviewVariant = "original" | "normalized";
type PreviewState = "loading" | "ready" | "failed";

export type PreviewAvailabilityLoader = (
  source: string,
  signal: AbortSignal,
) => Promise<void>;

function isPreviewContentType(contentType: string | null) {
  const mediaType = contentType?.split(";", 1)[0].trim().toLowerCase();
  return mediaType === "application/pdf" || mediaType?.startsWith("image/");
}

export const loadPreviewAvailability: PreviewAvailabilityLoader = async (
  source,
  signal,
) => {
  if (source.startsWith("data:")) return;

  const response = await fetch(source, {
    method: "GET",
    cache: "no-store",
    headers: { Range: "bytes=0-0" },
    signal,
  });
  const available =
    response.ok && isPreviewContentType(response.headers.get("Content-Type"));
  await response.body?.cancel();
  if (!available) throw new Error("Preview unavailable");
};

function withVariant(previewUrl: string, variant: PreviewVariant) {
  if (previewUrl.startsWith("data:")) return previewUrl;
  try {
    const url = new URL(previewUrl);
    url.searchParams.set("variant", variant);
    return url.toString();
  } catch {
    return previewUrl;
  }
}

export function ItemPreview({
  originalName,
  previewUrl,
  loadAvailability = loadPreviewAvailability,
}: ItemPreviewProps) {
  const hasNormalized = previewUrl.includes("variant=normalized");
  const [variant, setVariant] = useState<PreviewVariant>(
    hasNormalized ? "normalized" : "original",
  );
  const [state, setState] = useState<PreviewState>("loading");
  const [revision, setRevision] = useState(0);
  const source = useMemo(
    () => withVariant(previewUrl, variant),
    [previewUrl, variant],
  );

  useEffect(() => {
    const controller = new AbortController();
    let current = true;
    setState("loading");
    void loadAvailability(source, controller.signal).then(
      () => {
        if (current && !controller.signal.aborted) setState("ready");
      },
      () => {
        if (current && !controller.signal.aborted) setState("failed");
      },
    );
    return () => {
      current = false;
      controller.abort();
    };
  }, [loadAvailability, revision, source]);

  function chooseVariant(nextVariant: PreviewVariant) {
    setVariant(nextVariant);
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
        {state === "failed" ? (
          <div className="preview-error" role="alert">
            <FileWarning size={24} strokeWidth={1.6} aria-hidden="true" />
            <strong>无法显示票据预览</strong>
            <span>文件可能已移动、损坏或暂不支持此格式。</span>
            <button
              className="button button-secondary"
              type="button"
              onClick={() => {
                setRevision((current) => current + 1);
              }}
            >
              <RefreshCw size={15} strokeWidth={1.7} aria-hidden="true" />
              重新加载
            </button>
          </div>
        ) : state === "loading" ? (
          <div
            className="preview-loading"
            role="status"
            aria-label="正在检查票据预览"
          >
            正在加载预览
          </div>
        ) : (
          <iframe
            key={`${source}-${revision}`}
            title="票据预览"
            src={source}
            onError={() => setState("failed")}
          />
        )}
      </div>
    </section>
  );
}
