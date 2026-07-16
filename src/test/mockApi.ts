import { invoke, type InvokeArgs } from "@tauri-apps/api/core";
import { vi } from "vitest";

type CommandArguments = InvokeArgs | undefined;
type CommandHandler =
  | unknown
  | ((arguments_: CommandArguments) => unknown | Promise<unknown>);

const handlers = new Map<string, CommandHandler>();

export const invokeMock = vi.mocked(invoke);

export function resetMockApi() {
  handlers.clear();
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (command, arguments_) => {
    if (!handlers.has(command)) {
      throw new Error(`No mock registered for command: ${command}`);
    }

    const handler = handlers.get(command);
    return typeof handler === "function"
      ? (handler as (value: CommandArguments) => unknown | Promise<unknown>)(
          arguments_,
        )
      : handler;
  });
}

export function mockCommand<T>(
  command: string,
  response:
    | T
    | Promise<T>
    | ((arguments_: CommandArguments) => T | Promise<T>),
) {
  handlers.set(command, response);
}
