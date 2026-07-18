import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ItemPreview } from "./ItemPreview";

const fetchMock = vi.fn<typeof fetch>();

describe("ItemPreview", () => {
  beforeEach(() => {
    cleanup();
    fetchMock.mockReset();
    vi.stubGlobal("fetch", fetchMock);
  });

  afterEach(() => vi.unstubAllGlobals());

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

    expect(await screen.findByTitle("票据预览")).toHaveAttribute(
      "src",
      "invoice-file://item/invoice-taxi?variant=original",
    );
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("keeps a data URL preview usable without a network check", async () => {
    render(
      <ItemPreview
        originalName="qa-fixture.pdf"
        previewUrl="data:application/pdf;base64,JVBERi0xLjQ="
      />,
    );

    expect(await screen.findByTitle("票据预览")).toHaveAttribute(
      "src",
      "data:application/pdf;base64,JVBERi0xLjQ=",
    );
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it.each(["application/pdf", "image/png", "image/jpeg"])(
    "accepts a successful %s preview response",
    async (contentType) => {
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
        />,
      );

      if (contentType === "application/pdf") {
        expect(await screen.findByTitle("票据预览")).toBeInTheDocument();
      } else {
        expect(
          await screen.findByRole("img", { name: "票据预览" }),
        ).toBeInTheDocument();
      }
    },
  );

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
});
