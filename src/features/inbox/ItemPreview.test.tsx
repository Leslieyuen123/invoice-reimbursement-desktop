import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ItemPreview } from "./ItemPreview";

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

  afterEach(() => vi.unstubAllGlobals());

  it("contains preview layout and paint inside its allocated surface", () => {
    expect(appShellCss).toMatch(
      /\.item-preview-surface\s*\{[^}]*contain:\s*size layout paint;/s,
    );
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
});
