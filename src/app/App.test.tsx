import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { App } from "./App";

describe("App", () => {
  it("renders the invoice reimbursement heading", () => {
    render(<App />);

    expect(
      screen.getByRole("heading", { level: 1, name: "发票报销" }),
    ).toBeInTheDocument();
  });
});
