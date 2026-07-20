import { ExternalLink, FileArchive, FileWarning, RefreshCw } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import pdfWorkerUrl from "pdfjs-dist/legacy/build/pdf.worker.min.mjs?url";

import { api } from "../../lib/api";

interface ItemPreviewProps {
  itemId?: string;
  originalName: string;
  previewUrl: string;
  loadAvailability?: PreviewAvailabilityLoader;
  renderPdf?: PdfRenderer;
  openOriginal?: (itemId: string) => Promise<void>;
}

type PreviewVariant = "original" | "normalized";
type PreviewState = "loading" | "ready" | "failed";
type PreviewMediaType = "image" | "pdf" | "external";

export type PreviewAvailabilityLoader = (
  source: string,
  signal: AbortSignal,
) => Promise<PreviewMediaType | void>;

export type PdfRenderer = (
  surface: HTMLDivElement,
  source: string,
  signal: AbortSignal,
) => Promise<void>;

let pdfModulePromise:
  | Promise<typeof import("pdfjs-dist/legacy/build/pdf.mjs")>
  | undefined;

async function loadPdfModule() {
  pdfModulePromise ??= import("pdfjs-dist/legacy/build/pdf.mjs").then((pdfjs) => {
    pdfjs.GlobalWorkerOptions.workerSrc = pdfWorkerUrl;
    return pdfjs;
  });
  return pdfModulePromise;
}

export const renderPdfPages: PdfRenderer = async (surface, source, signal) => {
  const response = await fetch(source, { cache: "no-store", signal });
  if (!response.ok) throw new Error("Preview unavailable");
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (signal.aborted) throw new DOMException("Aborted", "AbortError");

  const pdfjs = await loadPdfModule();
  const loadingTask = pdfjs.getDocument({ data: bytes });
  const abort = () => void loadingTask.destroy();
  signal.addEventListener("abort", abort, { once: true });
  try {
    const pdfDocument = await loadingTask.promise;
    const availableWidth = Math.max(surface.clientWidth - 32, 320);
    const pixelRatio = Math.min(window.devicePixelRatio || 1, 2);
    for (let pageNumber = 1; pageNumber <= pdfDocument.numPages; pageNumber += 1) {
      if (signal.aborted) throw new DOMException("Aborted", "AbortError");
      const page = await pdfDocument.getPage(pageNumber);
      const unscaled = page.getViewport({ scale: 1 });
      const cssScale = Math.min(availableWidth / unscaled.width, 2);
      const viewport = page.getViewport({ scale: cssScale * pixelRatio });
      const canvas = window.document.createElement("canvas");
      const context = canvas.getContext("2d");
      if (!context) throw new Error("Preview unavailable");
      canvas.className = "item-preview-pdf-page";
      canvas.width = Math.ceil(viewport.width);
      canvas.height = Math.ceil(viewport.height);
      canvas.style.width = `${Math.ceil(viewport.width / pixelRatio)}px`;
      canvas.style.height = `${Math.ceil(viewport.height / pixelRatio)}px`;
      canvas.setAttribute("aria-label", `第 ${pageNumber} 页`);
      surface.append(canvas);
      await page.render({ canvas, canvasContext: context, viewport }).promise;
      page.cleanup();
    }
  } finally {
    signal.removeEventListener("abort", abort);
    await loadingTask.destroy();
  }
};

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
  if (/\.(?:url|zip|docx?|xlsx?)$/i.test(originalName.trim())) return "external";
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
  itemId,
  originalName,
  previewUrl,
  loadAvailability = loadPreviewAvailability,
  renderPdf = renderPdfPages,
  openOriginal = api.openItemOriginal,
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
  const [pdfReady, setPdfReady] = useState(false);
  const [openingOriginal, setOpeningOriginal] = useState(false);
  const pdfSurface = useRef<HTMLDivElement>(null);
  const source = useMemo(
    () => withVariant(previewUrl, variant),
    [previewUrl, variant],
  );

  useEffect(() => {
    const inferredMediaType = previewMediaTypeFromSource(originalName, source);
    if (inferredMediaType === "external") {
      setMediaType("external");
      setState("ready");
      return;
    }
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

  const isExternalUrl = /\.url$/i.test(originalName.trim());

  async function handleOpenOriginal() {
    if (!itemId || openingOriginal) return;
    setOpeningOriginal(true);
    try {
      await openOriginal(itemId);
    } finally {
      setOpeningOriginal(false);
    }
  }

  useEffect(() => {
    if (state !== "ready" || mediaType !== "pdf" || !pdfSurface.current) return;
    const surface = pdfSurface.current;
    const controller = new AbortController();
    let current = true;
    surface.replaceChildren();
    setPdfReady(false);
    void renderPdf(surface, source, controller.signal).then(
      () => {
        if (current && !controller.signal.aborted) setPdfReady(true);
      },
      () => {
        if (current && !controller.signal.aborted) setState("failed");
      },
    );
    return () => {
      current = false;
      controller.abort();
      surface.replaceChildren();
    };
  }, [mediaType, renderPdf, revision, source, state]);

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
        ) : mediaType === "external" ? (
          <div className="preview-external">
            {isExternalUrl ? (
              <ExternalLink size={28} strokeWidth={1.5} aria-hidden="true" />
            ) : (
              <FileArchive size={28} strokeWidth={1.5} aria-hidden="true" />
            )}
            <strong>{isExternalUrl ? "链接原件" : "系统文件"}</strong>
            <span>
              {isExternalUrl
                ? "此发票由邮件中的安全链接提供。"
                : "此格式需使用 Mac 上已安装的应用查看。"}
            </span>
            <button
              className="button button-secondary"
              type="button"
              disabled={!itemId || openingOriginal}
              onClick={() => void handleOpenOriginal()}
            >
              {isExternalUrl ? (
                <ExternalLink size={15} strokeWidth={1.7} aria-hidden="true" />
              ) : (
                <FileArchive size={15} strokeWidth={1.7} aria-hidden="true" />
              )}
              {isExternalUrl ? "在浏览器中打开原件" : "用系统应用打开原件"}
            </button>
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
          <div className="item-preview-pdf-shell">
            {!pdfReady ? (
              <div className="preview-loading" role="status">
                正在渲染 PDF
              </div>
            ) : null}
            <div
              key={`${source}-${revision}`}
              ref={pdfSurface}
              className="item-preview-pdf-pages"
              role="img"
              aria-label="票据预览"
            />
          </div>
        )}
      </div>
    </section>
  );
}
