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
  openOriginal?: OriginalOpener;
}

type PreviewVariant = "original" | "normalized";
type PreviewState = "loading" | "ready" | "failed";
type PreviewMediaType = "image" | "pdf" | "external";

interface PreviewAvailability {
  source: string;
  revision: number;
  state: PreviewState;
  mediaType: PreviewMediaType | null;
}

export type PreviewAvailabilityLoader = (
  source: string,
  signal: AbortSignal,
) => Promise<PreviewMediaType | void>;

export type PdfRenderer = (
  surface: HTMLDivElement,
  source: string,
  signal: AbortSignal,
) => Promise<void>;

type OriginalOpener = (itemId: string) => Promise<void>;

export interface PdfPageGeometry {
  width: number;
  height: number;
}

export interface PdfPagePlan {
  canvasWidth: number;
  canvasHeight: number;
  cssWidth: number;
  cssHeight: number;
  renderScale: number;
}

const MAX_PDF_PAGES = 100;
const MAX_CANVAS_DIMENSION = 8_192;
const MAX_PAGE_PIXELS = 16_000_000;
const MAX_TOTAL_PIXELS = 32_000_000;
const MAX_DEVICE_PIXEL_RATIO = 2;

function previewUnavailable(): never {
  throw new Error("Preview unavailable");
}

export function planPdfPages(
  pages: readonly PdfPageGeometry[],
  availableWidth: number,
  devicePixelRatio: number,
): PdfPagePlan[] {
  if (
    pages.length === 0 ||
    pages.length > MAX_PDF_PAGES ||
    !Number.isFinite(availableWidth) ||
    availableWidth <= 0 ||
    !Number.isFinite(devicePixelRatio) ||
    devicePixelRatio <= 0
  ) {
    previewUnavailable();
  }

  const pixelRatio = Math.min(devicePixelRatio, MAX_DEVICE_PIXEL_RATIO);
  let totalPixels = 0;

  return pages.map(({ width, height }) => {
    if (
      !Number.isFinite(width) ||
      width <= 0 ||
      !Number.isFinite(height) ||
      height <= 0
    ) {
      previewUnavailable();
    }

    const cssScale = Math.min(availableWidth / width, 2);
    const renderScale = cssScale * pixelRatio;
    const canvasWidth = Math.ceil(width * renderScale);
    const canvasHeight = Math.ceil(height * renderScale);
    const cssWidth = Math.ceil(canvasWidth / pixelRatio);
    const cssHeight = Math.ceil(canvasHeight / pixelRatio);

    if (
      !Number.isFinite(renderScale) ||
      renderScale <= 0 ||
      !Number.isSafeInteger(canvasWidth) ||
      canvasWidth <= 0 ||
      canvasWidth > MAX_CANVAS_DIMENSION ||
      !Number.isSafeInteger(canvasHeight) ||
      canvasHeight <= 0 ||
      canvasHeight > MAX_CANVAS_DIMENSION ||
      !Number.isSafeInteger(cssWidth) ||
      cssWidth <= 0 ||
      !Number.isSafeInteger(cssHeight) ||
      cssHeight <= 0
    ) {
      previewUnavailable();
    }

    const pagePixels = canvasWidth * canvasHeight;
    const nextTotalPixels = totalPixels + pagePixels;
    if (
      !Number.isSafeInteger(pagePixels) ||
      pagePixels > MAX_PAGE_PIXELS ||
      !Number.isSafeInteger(nextTotalPixels) ||
      nextTotalPixels > MAX_TOTAL_PIXELS
    ) {
      previewUnavailable();
    }
    totalPixels = nextTotalPixels;

    return {
      canvasWidth,
      canvasHeight,
      cssWidth,
      cssHeight,
      renderScale,
    };
  });
}

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

export function createPdfRenderer(
  loadModule: typeof loadPdfModule = loadPdfModule,
): PdfRenderer {
  return async (surface, source, signal) => {
    const response = await fetch(source, { cache: "no-store", signal });
    if (!response.ok) throw new Error("Preview unavailable");
    const bytes = new Uint8Array(await response.arrayBuffer());
    if (signal.aborted) throw new DOMException("Aborted", "AbortError");

    const pdfjs = await loadModule();
    if (signal.aborted) throw new DOMException("Aborted", "AbortError");

    const loadingTask = pdfjs.getDocument({ data: bytes });
    const destroyAbortedTask = async (): Promise<never> => {
      await loadingTask.destroy().catch(() => undefined);
      throw new DOMException("Aborted", "AbortError");
    };
    if (signal.aborted) return destroyAbortedTask();

    const abort = () => {
      void loadingTask.destroy().catch(() => undefined);
    };
    signal.addEventListener("abort", abort, { once: true });
    if (signal.aborted) {
      signal.removeEventListener("abort", abort);
      return destroyAbortedTask();
    }

    type PdfPage = Awaited<
      ReturnType<Awaited<typeof loadingTask.promise>["getPage"]>
    >;
    const pendingCleanup = new Set<PdfPage>();
    const pages: PdfPage[] = [];
    try {
      const pdfDocument = await loadingTask.promise;
      const availableWidth = Math.max(surface.clientWidth - 32, 320);
      if (
        !Number.isSafeInteger(pdfDocument.numPages) ||
        pdfDocument.numPages <= 0 ||
        pdfDocument.numPages > MAX_PDF_PAGES
      ) {
        previewUnavailable();
      }

      const geometries: PdfPageGeometry[] = [];
      for (
        let pageNumber = 1;
        pageNumber <= pdfDocument.numPages;
        pageNumber += 1
      ) {
        if (signal.aborted) throw new DOMException("Aborted", "AbortError");
        const page = await pdfDocument.getPage(pageNumber);
        pages.push(page);
        pendingCleanup.add(page);
        const unscaled = page.getViewport({ scale: 1 });
        geometries.push({ width: unscaled.width, height: unscaled.height });
      }

      const plans = planPdfPages(
        geometries,
        availableWidth,
        window.devicePixelRatio,
      );
      for (let index = 0; index < pages.length; index += 1) {
        if (signal.aborted) throw new DOMException("Aborted", "AbortError");
        const page = pages[index];
        const plan = plans[index];
        const viewport = page.getViewport({ scale: plan.renderScale });
        const canvas = window.document.createElement("canvas");
        const context = canvas.getContext("2d");
        if (!context) throw new Error("Preview unavailable");
        canvas.className = "item-preview-pdf-page";
        canvas.width = plan.canvasWidth;
        canvas.height = plan.canvasHeight;
        canvas.style.width = `${plan.cssWidth}px`;
        canvas.style.height = `${plan.cssHeight}px`;
        canvas.setAttribute("aria-label", `第 ${index + 1} 页`);
        surface.append(canvas);
        await page.render({ canvas, canvasContext: context, viewport }).promise;
        if (page.cleanup()) pendingCleanup.delete(page);
      }
    } finally {
      signal.removeEventListener("abort", abort);
      try {
        for (const page of pendingCleanup) page.cleanup();
      } finally {
        await loadingTask.destroy();
      }
    }
  };
}

export const renderPdfPages = createPdfRenderer();

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

interface OpenOriginalActionProps {
  itemId?: string;
  isExternalUrl: boolean;
  openOriginal: OriginalOpener;
  announceError?: boolean;
}

function OpenOriginalAction({
  itemId,
  isExternalUrl,
  openOriginal,
  announceError = true,
}: OpenOriginalActionProps) {
  const [opening, setOpening] = useState(false);
  const [openError, setOpenError] = useState(false);

  async function handleOpenOriginal() {
    if (!itemId || opening) return;
    setOpenError(false);
    setOpening(true);
    try {
      await openOriginal(itemId);
    } catch {
      setOpenError(true);
    } finally {
      setOpening(false);
    }
  }

  return (
    <>
      <button
        className="button button-secondary"
        type="button"
        disabled={!itemId || opening}
        onClick={() => void handleOpenOriginal()}
      >
        {isExternalUrl ? (
          <ExternalLink size={15} strokeWidth={1.7} aria-hidden="true" />
        ) : (
          <FileArchive size={15} strokeWidth={1.7} aria-hidden="true" />
        )}
        {isExternalUrl ? "在浏览器中打开原件" : "用系统应用打开原件"}
      </button>
      {openError ? (
        <span
          className="preview-open-error"
          role={announceError ? "alert" : undefined}
        >
          无法打开原件，请稍后重试。
        </span>
      ) : null}
    </>
  );
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
  const [revision, setRevision] = useState(0);
  const [pdfReady, setPdfReady] = useState(false);
  const pdfSurface = useRef<HTMLDivElement>(null);
  const source = useMemo(
    () => withVariant(previewUrl, variant),
    [previewUrl, variant],
  );
  const [availability, setAvailability] = useState<PreviewAvailability>(() => ({
    source,
    revision,
    state: "loading",
    mediaType: previewMediaTypeFromSource(originalName, source),
  }));
  const currentAvailability =
    availability.source === source && availability.revision === revision
      ? availability
      : null;
  const state = currentAvailability?.state ?? "loading";
  const mediaType = currentAvailability?.mediaType ?? null;

  useEffect(() => {
    const inferredMediaType = previewMediaTypeFromSource(originalName, source);
    if (inferredMediaType === "external") {
      setAvailability({
        source,
        revision,
        state: "ready",
        mediaType: "external",
      });
      return;
    }
    const controller = new AbortController();
    let current = true;
    setAvailability({
      source,
      revision,
      state: "loading",
      mediaType: inferredMediaType,
    });
    void loadAvailability(source, controller.signal).then(
      (detectedMediaType) => {
        if (current && !controller.signal.aborted) {
          setAvailability({
            source,
            revision,
            state: "ready",
            mediaType:
              detectedMediaType ??
              previewMediaTypeFromSource(originalName, source) ??
              "pdf",
          });
        }
      },
      () => {
        if (current && !controller.signal.aborted) {
          setAvailability({
            source,
            revision,
            state: "failed",
            mediaType: inferredMediaType,
          });
        }
      },
    );
    return () => {
      current = false;
      controller.abort();
    };
  }, [loadAvailability, originalName, revision, source]);

  const isExternalUrl = /\.url$/i.test(originalName.trim());

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
        if (current && !controller.signal.aborted) {
          setAvailability({
            source,
            revision,
            state: "failed",
            mediaType: "pdf",
          });
        }
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
            {itemId ? (
              <OpenOriginalAction
                key={source}
                itemId={itemId}
                isExternalUrl={isExternalUrl}
                openOriginal={openOriginal}
                announceError={false}
              />
            ) : null}
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
            <OpenOriginalAction
              key={source}
              itemId={itemId}
              isExternalUrl={isExternalUrl}
              openOriginal={openOriginal}
            />
          </div>
        ) : mediaType === "image" ? (
          <img
            key={`${source}-${revision}`}
            className="item-preview-image"
            alt="票据预览"
            src={source}
            onError={() =>
              setAvailability({
                source,
                revision,
                state: "failed",
                mediaType: "image",
              })
            }
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
