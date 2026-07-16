import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(() => new Promise(() => undefined)),
}));

import { App } from "./App";

describe("App", () => {
  it("renders the invoice reimbursement desktop shell", () => {
    render(<App />);

    expect(screen.getByLabelText("发票报销")).toBeInTheDocument();
    expect(screen.getByRole("navigation", { name: "主导航" })).toBeInTheDocument();
    expect(
      screen.getByRole("status", { name: "正在加载控制台" }),
    ).toBeInTheDocument();
  });
});
