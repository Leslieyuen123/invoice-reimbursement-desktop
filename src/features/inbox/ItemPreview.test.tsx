import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  createPdfRenderer,
  ItemPreview,
  planPdfPages,
} from "./ItemPreview";

type TestPdfRenderer = (
  surface: HTMLDivElement,
  source: string,
  signal: AbortSignal,
) => Promise<void>;

interface TestPdfModule {
  getDocument: (...args: unknown[]) => unknown;
}

function createTestPdfRenderer(
  loadModule: () => Promise<TestPdfModule>,
): TestPdfRenderer {
  return createPdfRenderer(
    loadModule as unknown as Parameters<typeof createPdfRenderer>[0],
  );
}

const fetchMock = vi.fn<typeof fetch>();
const appShellCss = readFileSync(
  resolve(process.cwd(), "src/app/AppShell.css"),
  "utf8",
);

describe("ItemPreview", () => {
  beforeEach(() => {
    cleanup();
    fetchMock.mockReset();
    vi.stubGlobal("fetch", fetchMock);
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("contains preview layout and paint inside its allocated surface", () => {
    expect(appShellCss).toMatch(
      /\.item-preview-surface\s*\{[^}]*contain:\s*size layout paint;/s,
    );
  });

  it("rejects 100 A4 pages when their cumulative physical pixels exceed the budget", () => {
    const pages = Array.from({ length: 100 }, () => ({
      width: 595,
      height: 842,
    }));

    expect(() => planPdfPages(pages, 1_000, 2)).toThrowError(
      "Preview unavailable",
    );
  });

  it.each([
    ["an extremely long MediaBox", { width: 100, height: 100_000 }, 1_000],
    ["an oversized square MediaBox", { width: 5_000, height: 5_000 }, 5_000],
  ])("rejects %s before allocating a canvas", (_label, page, availableWidth) => {
    expect(() => planPdfPages([page], availableWidth, 2)).toThrowError(
      "Preview unavailable",
    );
  });

  it("returns the exact canvas and CSS dimensions for a legal two-page PDF", () => {
    expect(
      planPdfPages(
        [
          { width: 600, height: 800 },
          { width: 300, height: 600 },
        ],
        600,
        2,
      ),
    ).toEqual([
      {
        canvasWidth: 1_200,
        canvasHeight: 1_600,
        cssWidth: 600,
        cssHeight: 800,
        renderScale: 2,
      },
      {
        canvasWidth: 1_200,
        canvasHeight: 2_400,
        cssWidth: 600,
        cssHeight: 1_200,
        renderScale: 4,
      },
    ]);
  });

  it.each([
    ["an empty document", [], 600, 2],
    [
      "more than 100 pages",
      Array.from({ length: 101 }, () => ({ width: 600, height: 800 })),
      600,
      1,
    ],
    [
      "a non-finite page width",
      [{ width: Number.POSITIVE_INFINITY, height: 800 }],
      600,
      1,
    ],
    ["a non-positive page height", [{ width: 600, height: 0 }], 600, 1],
    [
      "a non-finite available width",
      [{ width: 600, height: 800 }],
      Number.NaN,
      1,
    ],
    ["a non-positive pixel ratio", [{ width: 600, height: 800 }], 600, 0],
  ])("rejects %s", (_label, pages, availableWidth, devicePixelRatio) => {
    expect(() =>
      planPdfPages(pages, availableWidth, devicePixelRatio),
    ).toThrowError("Preview unavailable");
  });

  it("cleans each rendered page before rendering the next page", async () => {
    fetchMock.mockResolvedValue(new Response(new Uint8Array([0x25, 0x50])));
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(
      {} as CanvasRenderingContext2D,
    );
    const firstCleanup = vi.fn(() => true);
    const firstPage = {
      cleanup: firstCleanup,
      getViewport: vi.fn(({ scale }: { scale: number }) => ({
        width: 100 * scale,
        height: 100 * scale,
      })),
      render: vi.fn(() => ({ promise: Promise.resolve() })),
    };
    const secondPage = {
      cleanup: vi.fn(() => true),
      getViewport: vi.fn(({ scale }: { scale: number }) => ({
        width: 100 * scale,
        height: 100 * scale,
      })),
      render: vi.fn(() => {
        expect(firstCleanup).toHaveBeenCalledTimes(1);
        return { promise: Promise.resolve() };
      }),
    };
    const pages = [firstPage, secondPage];
    const destroy = vi.fn().mockResolvedValue(undefined);
    const getDocument = vi.fn(() => ({
      destroy,
      promise: Promise.resolve({
        numPages: pages.length,
        getPage: vi.fn(async (pageNumber: number) => pages[pageNumber - 1]),
      }),
    }));
    const renderer = createTestPdfRenderer(
      vi.fn().mockResolvedValue({ getDocument }),
    );

    await renderer(
      document.createElement("div"),
      "invoice-file://item/two-pages",
      new AbortController().signal,
    );

    expect(firstCleanup).toHaveBeenCalledTimes(1);
    expect(secondPage.cleanup).toHaveBeenCalledTimes(1);
    expect(destroy).toHaveBeenCalledTimes(1);
  });

  it("does not create a PDF loading task when aborted during module loading", async () => {
    fetchMock.mockResolvedValue(new Response(new Uint8Array([0x25, 0x50])));
    const getDocument = vi.fn();
    let resolveModule!: (module: TestPdfModule) => void;
    const loadModule = vi.fn(
      () =>
        new Promise<TestPdfModule>((resolve) => {
          resolveModule = resolve;
        }),
    );
    const renderer = createTestPdfRenderer(loadModule);
    const controller = new AbortController();
    const renderPromise = renderer(
      document.createElement("div"),
      "invoice-file://item/aborted-module",
      controller.signal,
    );
    const rejection = expect(renderPromise).rejects.toMatchObject({
      name: "AbortError",
    });
    await waitFor(() => expect(loadModule).toHaveBeenCalledTimes(1));

    controller.abort();
    resolveModule({ getDocument });

    await rejection;
    expect(getDocument).not.toHaveBeenCalled();
  });

  it("destroys a loading task when aborted before its listener is bound", async () => {
    fetchMock.mockResolvedValue(new Response(new Uint8Array([0x25, 0x50])));
    const controller = new AbortController();
    const destroy = vi.fn().mockResolvedValue(undefined);
    const getDocument = vi.fn(() => {
      controller.abort();
      return { destroy, promise: new Promise(() => undefined) };
    });
    const renderer = createTestPdfRenderer(
      vi.fn().mockResolvedValue({ getDocument }),
    );

    await expect(
      renderer(
        document.createElement("div"),
        "invoice-file://item/aborted-task",
        controller.signal,
      ),
    ).rejects.toMatchObject({ name: "AbortError" });
    expect(destroy).toHaveBeenCalledTimes(1);
  });

  it("aborts an unfinished preview check when it unmounts", async () => {
    let requestSignal: AbortSignal | undefined;
    fetchMock.mockImplementation(
      (_input, init) =>
        new Promise<Response>(() => {
          requestSignal = init?.signal ?? undefined;
        }),
    );

    const view = render(
      <ItemPreview
        originalName="出租车电子发票.pdf"
        previewUrl="invoice-file://item/invoice-taxi?variant=normalized"
      />,
    );
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));

    view.unmount();

    expect(requestSignal?.aborted).toBe(true);
  });

  it("ignores a stale failed check after switching preview variants", async () => {
    const user = userEvent.setup();
    const renderPdf = vi.fn().mockResolvedValue(undefined);
    const resolvers: Array<(response: Response) => void> = [];
    fetchMock.mockImplementation(
      () =>
        new Promise<Response>((resolve) => {
          resolvers.push(resolve);
        }),
    );

    render(
      <ItemPreview
        originalName="出租车电子发票.pdf"
        previewUrl="invoice-file://item/invoice-taxi?variant=normalized"
        renderPdf={renderPdf}
      />,
    );
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    await user.click(screen.getByRole("button", { name: "原件" }));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));

    await act(async () => {
      resolvers[0](
        new Response("Preview unavailable", {
          status: 404,
          headers: { "Content-Type": "text/plain" },
        }),
      );
      resolvers[1](
        new Response(new Uint8Array(), {
          status: 200,
          headers: { "Content-Type": "application/pdf" },
        }),
      );
    });

    expect(await screen.findByRole("img", { name: "票据预览" })).toBeInTheDocument();
    await waitFor(() => expect(renderPdf).toHaveBeenCalledTimes(1));
    expect(renderPdf).toHaveBeenCalledWith(
      expect.any(HTMLDivElement),
      "invoice-file://item/invoice-taxi?variant=original",
      expect.any(AbortSignal),
    );
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("waits for the new source availability before rendering its PDF", async () => {
    const user = userEvent.setup();
    const renderPdf = vi.fn().mockResolvedValue(undefined);
    const availabilityResolvers: Array<(mediaType: "pdf") => void> = [];
    const loadAvailability = vi.fn(
      () =>
        new Promise<"pdf">((resolve) => {
          availabilityResolvers.push(resolve);
        }),
    );

    render(
      <ItemPreview
        originalName="invoice.pdf"
        previewUrl="invoice-file://item/invoice?variant=normalized"
        loadAvailability={loadAvailability}
        renderPdf={renderPdf}
      />,
    );
    await waitFor(() => expect(loadAvailability).toHaveBeenCalledTimes(1));
    await act(async () => availabilityResolvers[0]("pdf"));
    await waitFor(() => expect(renderPdf).toHaveBeenCalledTimes(1));

    await user.click(screen.getByRole("button", { name: "原件" }));
    await waitFor(() => expect(loadAvailability).toHaveBeenCalledTimes(2));

    expect(renderPdf).toHaveBeenCalledTimes(1);

    await act(async () => availabilityResolvers[1]("pdf"));
    await waitFor(() => expect(renderPdf).toHaveBeenCalledTimes(2));
    expect(renderPdf).toHaveBeenLastCalledWith(
      expect.any(HTMLDivElement),
      "invoice-file://item/invoice?variant=original",
      expect.any(AbortSignal),
    );
  });

  it("keeps a data URL preview usable without a network check", async () => {
    const renderPdf = vi.fn().mockResolvedValue(undefined);
    render(
      <ItemPreview
        originalName="qa-fixture.pdf"
        previewUrl="data:application/pdf;base64,JVBERi0xLjQ="
        renderPdf={renderPdf}
      />,
    );

    expect(await screen.findByRole("img", { name: "票据预览" })).toBeInTheDocument();
    await waitFor(() => expect(renderPdf).toHaveBeenCalledWith(
      expect.any(HTMLDivElement),
      "data:application/pdf;base64,JVBERi0xLjQ=",
      expect.any(AbortSignal),
    ));
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it.each(["application/pdf", "image/png", "image/jpeg"])(
    "accepts a successful %s preview response",
    async (contentType) => {
      const renderPdf = vi.fn().mockResolvedValue(undefined);
      fetchMock.mockResolvedValue(
        new Response(new Uint8Array(), {
          status: 200,
          headers: { "Content-Type": contentType },
        }),
      );

      render(
        <ItemPreview
          originalName="preview-fixture"
          previewUrl="invoice-file://item/invoice-taxi?variant=original"
          renderPdf={renderPdf}
        />,
      );

      if (contentType === "application/pdf") {
        expect(
          await screen.findByRole("img", { name: "票据预览" }),
        ).toBeInTheDocument();
        await waitFor(() => expect(renderPdf).toHaveBeenCalledTimes(1));
      } else {
        expect(
          await screen.findByRole("img", { name: "票据预览" }),
        ).toBeInTheDocument();
      }
    },
  );

  it("renders PDF bytes into a canvas instead of a WebKit iframe", async () => {
    const renderPdf = vi.fn().mockResolvedValue(undefined);
    fetchMock.mockResolvedValue(
      new Response(new Uint8Array([0x25, 0x50, 0x44, 0x46]), {
        status: 200,
        headers: { "Content-Type": "application/pdf" },
      }),
    );

    render(
      <ItemPreview
        originalName="invoice.pdf"
        previewUrl="invoice-file://item/invoice-pdf?variant=original"
        renderPdf={renderPdf}
      />,
    );

    const canvas = await screen.findByRole("img", { name: "票据预览" });
    await waitFor(() => expect(renderPdf).toHaveBeenCalledTimes(1));
    expect(renderPdf).toHaveBeenCalledWith(
      canvas,
      "invoice-file://item/invoice-pdf?variant=original",
      expect.any(AbortSignal),
    );
    expect(screen.queryByTitle("票据预览")).not.toBeInTheDocument();
  });

  it.each(["receipt.JPG", "receipt.jpeg", "receipt.png"])(
    "renders image ticket %s as a contained image instead of an iframe",
    async (originalName) => {
      render(
        <ItemPreview
          originalName={originalName}
          previewUrl="invoice-file://item/image-ticket?variant=original"
          loadAvailability={vi.fn().mockResolvedValue(undefined)}
        />,
      );

      const preview = await screen.findByRole("img", { name: "票据预览" });
      expect(preview).toHaveAttribute(
        "src",
        "invoice-file://item/image-ticket?variant=original",
      );
      expect(preview).toHaveClass("item-preview-image");
      expect(screen.queryByTitle("票据预览")).not.toBeInTheDocument();
    },
  );

  it("uses the response MIME type when the image URL and name have no extension", async () => {
    fetchMock.mockResolvedValue(
      new Response(new Uint8Array(), {
        status: 200,
        headers: { "Content-Type": "image/png" },
      }),
    );

    render(
      <ItemPreview
        originalName="scanned-ticket"
        previewUrl="invoice-file://item/scanned-ticket?variant=original"
      />,
    );

    expect(
      await screen.findByRole("img", { name: "票据预览" }),
    ).toBeInTheDocument();
  });

  it("rejects a successful textual preview response", async () => {
    fetchMock.mockResolvedValue(
      new Response("Preview unavailable", {
        status: 200,
        headers: { "Content-Type": "text/plain; charset=utf-8" },
      }),
    );

    render(
      <ItemPreview
        originalName="preview-fixture"
        previewUrl="invoice-file://item/invoice-taxi?variant=original"
      />,
    );

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "无法显示票据预览",
    );
    expect(screen.queryByTitle("票据预览")).not.toBeInTheDocument();
  });

  it.each([
    ["download-link.url", "在浏览器中打开原件"],
    ["invoice-bundle.zip", "用系统应用打开原件"],
  ])("offers a usable original action for %s", async (originalName, label) => {
    const user = userEvent.setup();
    const openOriginal = vi.fn().mockResolvedValue(undefined);

    render(
      <ItemPreview
        itemId="item-external"
        originalName={originalName}
        previewUrl="invoice-file://item/item-external?variant=original"
        openOriginal={openOriginal}
      />,
    );

    await user.click(await screen.findByRole("button", { name: label }));

    expect(openOriginal).toHaveBeenCalledWith("item-external");
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("offers the original when PDF rendering fails", async () => {
    const user = userEvent.setup();
    const openOriginal = vi.fn().mockResolvedValue(undefined);

    render(
      <ItemPreview
        itemId="item-pdf"
        originalName="invoice.pdf"
        previewUrl="invoice-file://item/item-pdf?variant=original"
        loadAvailability={vi.fn().mockResolvedValue("pdf")}
        renderPdf={vi.fn().mockRejectedValue(new Error("render failed"))}
        openOriginal={openOriginal}
      />,
    );

    const openButton = await screen.findByRole("button", {
      name: "用系统应用打开原件",
    });
    await user.click(openButton);

    expect(openOriginal).toHaveBeenCalledWith("item-pdf");
  });

  it("uses the failed PDF live region for an opener error without nesting alerts", async () => {
    const user = userEvent.setup();
    const openOriginal = vi.fn().mockRejectedValue(new Error("open failed"));

    render(
      <ItemPreview
        itemId="item-pdf"
        originalName="invoice.pdf"
        previewUrl="invoice-file://item/item-pdf?variant=original"
        loadAvailability={vi.fn().mockResolvedValue("pdf")}
        renderPdf={vi.fn().mockRejectedValue(new Error("render failed"))}
        openOriginal={openOriginal}
      />,
    );

    await user.click(
      await screen.findByRole("button", { name: "用系统应用打开原件" }),
    );

    const alerts = await screen.findAllByRole("alert");
    expect(alerts).toHaveLength(1);
    expect(alerts[0]).toHaveTextContent("无法打开原件，请稍后重试。");
  });

  it("surfaces a generic opener error and re-enables retry", async () => {
    const user = userEvent.setup();
    const openOriginal = vi
      .fn()
      .mockRejectedValue(new Error("backend path /private/invoices/secret.pdf"));

    render(
      <ItemPreview
        itemId="item-external"
        originalName="invoice-bundle.zip"
        previewUrl="invoice-file://item/item-external?variant=original"
        openOriginal={openOriginal}
      />,
    );

    const openButton = await screen.findByRole("button", {
      name: "用系统应用打开原件",
    });
    await user.click(openButton);

    const error = await screen.findByRole("alert");
    expect(error).toHaveTextContent("无法打开原件，请稍后重试。");
    expect(error).not.toHaveTextContent("/private/invoices/secret.pdf");
    expect(openButton).toBeEnabled();

    openOriginal.mockResolvedValueOnce(undefined);
    await user.click(openButton);

    await waitFor(() =>
      expect(screen.queryByRole("alert")).not.toBeInTheDocument(),
    );
    expect(openOriginal).toHaveBeenCalledTimes(2);
  });

  it("clears a stale opener error when the preview source changes", async () => {
    const user = userEvent.setup();
    const openOriginal = vi.fn().mockRejectedValue(new Error("open failed"));

    render(
      <ItemPreview
        itemId="item-external"
        originalName="invoice-bundle.zip"
        previewUrl="invoice-file://item/item-external?variant=normalized"
        openOriginal={openOriginal}
      />,
    );

    await user.click(
      await screen.findByRole("button", { name: "用系统应用打开原件" }),
    );
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "无法打开原件，请稍后重试。",
    );

    await user.click(screen.getByRole("button", { name: "原件" }));

    await waitFor(() =>
      expect(screen.queryByRole("alert")).not.toBeInTheDocument(),
    );
  });
});
