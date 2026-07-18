import { FileWarning, RefreshCw } from "lucide-react";
import { useEffect, useMemo, useState } from "react";

interface ItemPreviewProps {
  originalName: string;
  previewUrl: string;
  loadAvailability?: PreviewAvailabilityLoader;
}

type PreviewVariant = "original" | "normalized";
type PreviewState = "loading" | "ready" | "failed";
type PreviewMediaType = "image" | "pdf";

export type PreviewAvailabilityLoader = (
  source: string,
  signal: AbortSignal,
) => Promise<PreviewMediaType | void>;

function previewMediaTypeFromContentType(
  contentType: string | null,
): PreviewMediaType | null {
  const mediaType = contentType?.split(";", 1)[0].trim().toLowerCase();
  if (mediaType === "application/pdf") return "pdf";
  if (mediaType === "image/jpeg" || mediaType === "image/png") return "image";
  return null;
}

function previewMediaTypeFromSource(originalName: string, source: string) {
  if (/\.(?:jpe?g|png)$/i.test(originalName.trim())) return "image";
  if (/\.pdf$/i.test(originalName.trim())) return "pdf";
  if (source.startsWith("data:")) {
    return previewMediaTypeFromContentType(source.slice(5).split(/[;,]/, 1)[0]);
  }
  return null;
}

export const loadPreviewAvailability: PreviewAvailabilityLoader = async (
  source,
  signal,
) => {
  if (source.startsWith("data:")) {
    const mediaType = previewMediaTypeFromContentType(
      source.slice(5).split(/[;,]/, 1)[0],
    );
    if (!mediaType) throw new Error("Preview unavailable");
    return mediaType;
  }

  const response = await fetch(source, {
    method: "GET",
    cache: "no-store",
    headers: { Range: "bytes=0-0" },
    signal,
  });
  const mediaType = previewMediaTypeFromContentType(
    response.headers.get("Content-Type"),
  );
  const available = response.ok && mediaType !== null;
  await response.body?.cancel();
  if (!available) throw new Error("Preview unavailable");
  return mediaType;
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
  const [mediaType, setMediaType] = useState<PreviewMediaType | null>(() =>
    previewMediaTypeFromSource(originalName, previewUrl),
  );
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
      (detectedMediaType) => {
        if (current && !controller.signal.aborted) {
          setMediaType(
            detectedMediaType ??
              previewMediaTypeFromSource(originalName, source) ??
              "pdf",
          );
          setState("ready");
        }
      },
      () => {
        if (current && !controller.signal.aborted) setState("failed");
      },
    );
    return () => {
      current = false;
      controller.abort();
    };
  }, [loadAvailability, originalName, revision, source]);

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
        ) : mediaType === "image" ? (
          <img
            key={`${source}-${revision}`}
            className="item-preview-image"
            alt="票据预览"
            src={source}
            onError={() => setState("failed")}
          />
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
