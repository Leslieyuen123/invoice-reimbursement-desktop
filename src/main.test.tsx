import { screen, waitFor } from "@testing-library/dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { installBrowserCommandBridgeMock } = vi.hoisted(() => ({
  installBrowserCommandBridgeMock: vi.fn(() => {
    throw new Error("secret app password: do-not-render");
  }),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(() => new Promise(() => undefined)),
}));
vi.mock("./test/browserCommandBridge", () => ({
  installBrowserCommandBridge: installBrowserCommandBridgeMock,
}));

describe("desktop bootstrap", () => {
  beforeEach(() => {
    document.body.innerHTML = '<div id="root"></div>';
    vi.stubEnv("VITE_BROWSER_COMMAND_BRIDGE", "1");
  });

  afterEach(() => {
    document.body.innerHTML = "";
    vi.unstubAllEnvs();
  });

  it("renders the fatal fallback when the real top-level bootstrap rejects", async () => {
    await import("./main");

    await waitFor(() => expect(installBrowserCommandBridgeMock).toHaveBeenCalled());
    expect(await screen.findByRole("alert")).toHaveTextContent("应用无法启动");
    expect(screen.getByRole("alert")).not.toHaveTextContent("do-not-render");
  });
});
