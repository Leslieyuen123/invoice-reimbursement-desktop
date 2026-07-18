import { screen } from "@testing-library/dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(() => new Promise(() => undefined)),
}));

describe("desktop bootstrap", () => {
  beforeEach(() => {
    document.body.innerHTML = '<div id="root"></div>';
  });

  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("renders a generic visible alert without leaking bootstrap error details", async () => {
    const mainModule = (await import("./main")) as typeof import("./main") & {
      renderBootstrapFailure?: (root: HTMLElement, error: unknown) => void;
    };
    const root = document.getElementById("root");
    if (!root) throw new Error("test root missing");

    expect(mainModule.renderBootstrapFailure).toBeTypeOf("function");
    mainModule.renderBootstrapFailure?.(
      root,
      new Error("secret app password: do-not-render"),
    );

    expect(screen.getByRole("alert")).toHaveTextContent("应用无法启动");
    expect(screen.getByRole("alert")).not.toHaveTextContent("do-not-render");
  });
});
