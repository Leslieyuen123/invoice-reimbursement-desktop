import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { onDragDropEventMock, openDialogMock } = vi.hoisted(() => ({
  onDragDropEventMock: vi.fn(),
  openDialogMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({ onDragDropEvent: onDragDropEventMock }),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: openDialogMock }));

import { FileDropZone } from "./FileDropZone";

describe("FileDropZone", () => {
  beforeEach(() => {
    cleanup();
    onDragDropEventMock.mockReset();
    openDialogMock.mockReset();
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllEnvs();
    Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
  });

  it("uses an accessible browser file input only when the explicit bridge flag is enabled", async () => {
    vi.stubEnv("VITE_BROWSER_COMMAND_BRIDGE", "1");
    const user = userEvent.setup();
    const onPaths = vi.fn();
    onDragDropEventMock.mockResolvedValue(vi.fn());

    render(<FileDropZone onPaths={onPaths} />);
    const input = screen.getByLabelText("选择票据文件");
    const file = new File(["invoice"], "text-invoice.pdf", {
      type: "application/pdf",
    });
    await user.upload(input, file);

    expect(onPaths).toHaveBeenCalledWith(["text-invoice.pdf"]);
    expect(openDialogMock).not.toHaveBeenCalled();
  });

  it("shows a retryable error when the native file dialog rejects", async () => {
    const user = userEvent.setup();
    const onPaths = vi.fn();
    onDragDropEventMock.mockResolvedValue(vi.fn());
    openDialogMock
      .mockRejectedValueOnce(new Error("dialog unavailable"))
      .mockResolvedValueOnce(["/Users/finance/retry.pdf"]);

    render(<FileDropZone onPaths={onPaths} />);
    await user.click(screen.getByRole("button", { name: "选择文件" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "无法打开文件选择器，请重试",
    );
    await user.click(screen.getByRole("button", { name: "选择文件" }));
    expect(onPaths).toHaveBeenCalledWith(["/Users/finance/retry.pdf"]);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("shows a clearable error when drag listener registration rejects", async () => {
    const user = userEvent.setup();
    onDragDropEventMock.mockRejectedValue(new Error("listener unavailable"));

    render(<FileDropZone onPaths={vi.fn()} />);

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "拖放导入暂不可用，请使用选择文件",
    );
    await user.click(screen.getByRole("button", { name: "清除上传错误" }));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("registers once, uses the latest callback state, and cleans up", async () => {
    let dragHandler: ((event: { payload: unknown }) => void) | undefined;
    const unlisten = vi.fn();
    onDragDropEventMock.mockImplementation(
      async (handler: (event: { payload: unknown }) => void) => {
        dragHandler = handler;
        return unlisten;
      },
    );
    const firstOnPaths = vi.fn();
    const latestOnPaths = vi.fn();
    const view = render(<FileDropZone onPaths={firstOnPaths} />);
    await waitFor(() => expect(onDragDropEventMock).toHaveBeenCalledTimes(1));

    view.rerender(<FileDropZone disabled onPaths={latestOnPaths} />);
    await act(async () => {
      dragHandler?.({ payload: { type: "drop", paths: ["/tmp/ignored.pdf"] } });
    });
    expect(firstOnPaths).not.toHaveBeenCalled();
    expect(latestOnPaths).not.toHaveBeenCalled();

    view.rerender(<FileDropZone onPaths={latestOnPaths} />);
    await act(async () => {
      dragHandler?.({ payload: { type: "drop", paths: ["/tmp/latest.pdf"] } });
    });
    expect(onDragDropEventMock).toHaveBeenCalledTimes(1);
    expect(latestOnPaths).toHaveBeenCalledWith(["/tmp/latest.pdf"]);

    view.unmount();
    expect(unlisten).toHaveBeenCalledTimes(1);
  });
});
