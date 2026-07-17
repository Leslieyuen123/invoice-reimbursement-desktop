import type {
  AppError,
  BatchCandidateDto,
  BatchDetailDto,
  BatchDetailSummaryDto,
  BatchDto,
  Category,
  DashboardDto,
  ExportResultDto,
  InvoiceItemDto,
  ItemFilter,
  MailboxAccountDto,
  ManualImportOutcomeDto,
  NewBatchInputDto,
  PageDto,
  PreferencesDto,
  ReviewItemInputDto,
  SaveMailboxAccountInputDto,
  StorageStatusDto,
} from "../types";

type CommandArguments = Record<string, unknown>;
export type BrowserCommandBridge = <T>(
  command: string,
  arguments_?: CommandArguments,
) => Promise<T>;

interface BridgeState {
  items: InvoiceItemDto[];
  batches: BatchDto[];
  accounts: MailboxAccountDto[];
  preferences: PreferencesDto;
  storage: StorageStatusDto;
  nextItemId: number;
  nextBatchId: number;
  nextAccountId: number;
}

type CommandHandler = (arguments_: CommandArguments) => unknown | Promise<unknown>;

interface BrowserBridgeOptions {
  delayMs?: number;
  failCommands?: ReadonlySet<string>;
}

const now = "2026-07-17T08:00:00Z";
const previewFixtureUrl = "/src-tauri/tests/fixtures/image-invoice.png";

function notFound(entity: string): AppError {
  return { code: "not_found", entity, message: `${entity} not found` };
}

function requiredString(arguments_: CommandArguments, name: string) {
  const value = arguments_[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`Browser command argument ${name} must be a string`);
  }
  return value;
}

function requiredObject<T>(arguments_: CommandArguments, name: string): T {
  const value = arguments_[name];
  if (typeof value !== "object" || value === null) {
    throw new Error(`Browser command argument ${name} must be an object`);
  }
  return value as T;
}

function page<T>(items: T[]): PageDto<T> {
  return { items, nextCursor: null };
}

function initialState(): BridgeState {
  return {
    items: [],
    batches: [],
    accounts: [],
    preferences: {
      backgroundSyncEnabled: true,
      exportDirectory: "/tmp/invoice-reimbursement/e2e",
      batchDirectoryPattern: "{batchName}-{timestamp}",
    },
    storage: {
      localDataDirectory: "/tmp/invoice-reimbursement/e2e/data",
      exportDirectory: "/tmp/invoice-reimbursement/e2e",
      availableBytes: 8_589_934_592,
      recoveryError: null,
    },
    nextItemId: 1,
    nextBatchId: 1,
    nextAccountId: 1,
  };
}

function categorySummary(items: InvoiceItemDto[], category: Category) {
  const matching = items.filter((item) => item.finalCategory === category);
  return {
    itemCount: matching.length,
    amountCents: matching.reduce((total, item) => total + (item.amountCents ?? 0), 0),
  };
}

function batchDetail(state: BridgeState, batchId: string): BatchDetailDto {
  const batch = state.batches.find((candidate) => candidate.id === batchId);
  if (!batch) throw notFound("batch");
  const items = state.items.filter((item) => item.batchId === batchId);
  const summary: BatchDetailSummaryDto = {
    itemCount: items.length,
    totalAmountCents: items.reduce(
      (total, item) => total + (item.amountCents ?? 0),
      0,
    ),
    transport: categorySummary(items, "transport"),
    dining: categorySummary(items, "dining"),
    accommodation: categorySummary(items, "accommodation"),
    hospitality: categorySummary(items, "hospitality"),
    unconfirmedCount: items.filter((item) => item.confirmationStatus !== "confirmed")
      .length,
  };
  Object.assign(batch, {
    itemCount: summary.itemCount,
    totalAmountCents: summary.totalAmountCents,
    unconfirmedCount: summary.unconfirmedCount,
    updatedAt: now,
  });
  return { batch: { ...batch }, items: items.map((item) => ({ ...item })), summary, warnings: [] };
}

function itemMatches(item: InvoiceItemDto, filter: ItemFilter) {
  const query = filter.query?.trim().toLocaleLowerCase("zh-CN");
  return (
    (!filter.status || item.status === filter.status) &&
    (!filter.suggestedPeriod || item.suggestedPeriod === filter.suggestedPeriod) &&
    (!filter.category || item.finalCategory === filter.category) &&
    (!filter.sourceType || item.sourceType === filter.sourceType) &&
    (!filter.batchId || item.batchId === filter.batchId) &&
    (!query ||
      [item.originalName, item.company, item.city]
        .filter((value): value is string => Boolean(value))
        .some((value) => value.toLocaleLowerCase("zh-CN").includes(query)))
  );
}

function createItem(state: BridgeState, path: string): InvoiceItemDto {
  const originalName = path.split(/[\\/]/).at(-1) || path;
  const item: InvoiceItemDto = {
    id: `browser-item-${state.nextItemId++}`,
    originalName,
    previewUrl: previewFixtureUrl,
    sourceType: "manual_upload",
    sourceAccountId: null,
    fetchedAt: now,
    invoiceDate: "2026-07-15",
    suggestedPeriod: "2026-07",
    batchId: null,
    suggestedCategory: "dining",
    finalCategory: null,
    amountCents: 12_850,
    currency: "CNY",
    city: "上海",
    company: "测试餐厅",
    status: "pending_confirmation",
    recognitionStatus: "succeeded",
    confirmationStatus: "pending",
    dedupeStatus: "unique",
    note: null,
    eventTag: null,
    projectTag: null,
    createdAt: now,
    updatedAt: now,
  };
  state.items.unshift(item);
  return item;
}

function dashboard(state: BridgeState): DashboardDto {
  return {
    mailboxAccounts: state.accounts.map((account) => ({ ...account })),
    recentlyAddedCount: state.items.length,
    pendingConfirmationCount: state.items.filter(
      (item) => item.status === "pending_confirmation",
    ).length,
    recognitionFailedCount: state.items.filter(
      (item) => item.status === "recognition_failed",
    ).length,
    suspectedDuplicateCount: state.items.filter(
      (item) => item.status === "suspected_duplicate",
    ).length,
    recentBatches: state.batches.slice(0, 5).map((batch) => ({ ...batch })),
  };
}

function makeHandlers(state: BridgeState): Map<string, CommandHandler> {
  const handlers: Array<[string, CommandHandler]> = [
    ["get_dashboard", () => dashboard(state)],
    ["list_items", (arguments_) => {
      const filter = (arguments_.filter ?? {}) as ItemFilter;
      return page(state.items.filter((item) => itemMatches(item, filter)).map((item) => ({ ...item })));
    }],
    ["get_item", (arguments_) => {
      const item = state.items.find(
        (candidate) => candidate.id === requiredString(arguments_, "itemId"),
      );
      if (!item) throw notFound("item");
      return { ...item };
    }],
    ["import_manual_files", (arguments_) => {
      const paths = arguments_.paths;
      if (!Array.isArray(paths) || !paths.every((path) => typeof path === "string")) {
        throw new Error("Browser command argument paths must be a string array");
      }
      return paths.map<ManualImportOutcomeDto>((path) => ({
        status: "imported",
        path,
        item: { ...createItem(state, path) },
      }));
    }],
    ["review_item", (arguments_) => {
      const input = requiredObject<ReviewItemInputDto>(arguments_, "input");
      const item = state.items.find((candidate) => candidate.id === input.id);
      if (!item) throw notFound("item");
      Object.assign(item, input, {
        confirmationStatus: "confirmed",
        recognitionStatus: "succeeded",
        status: "ready",
        updatedAt: now,
      });
      return { ...item };
    }],
    ["resolve_duplicate", (arguments_) => {
      const itemId = requiredString(arguments_, "itemId");
      const item = state.items.find((candidate) => candidate.id === itemId);
      if (!item) throw notFound("item");
      if (arguments_.keep === false) {
        state.items = state.items.filter((candidate) => candidate.id !== itemId);
        return null;
      }
      Object.assign(item, { dedupeStatus: "resolved", status: "pending_confirmation", updatedAt: now });
      return { ...item };
    }],
    ["retry_recognition", (arguments_) => {
      const item = state.items.find(
        (candidate) => candidate.id === requiredString(arguments_, "itemId"),
      );
      if (!item) throw notFound("item");
      Object.assign(item, { recognitionStatus: "succeeded", status: "pending_confirmation", updatedAt: now });
      return { ...item };
    }],
    ["list_batches", () => page(state.batches.map((batch) => ({ ...batch })))],
    ["get_batch", (arguments_) => batchDetail(state, requiredString(arguments_, "batchId"))],
    ["list_batch_candidates", (arguments_) => {
      const batchId = requiredString(arguments_, "batchId");
      const batch = state.batches.find((candidate) => candidate.id === batchId);
      if (!batch) throw notFound("batch");
      const query = typeof arguments_.query === "string" ? arguments_.query.toLocaleLowerCase("zh-CN") : "";
      const candidates = state.items
        .filter((item) => item.batchId === null && (!query || item.originalName.toLocaleLowerCase("zh-CN").includes(query)))
        .map<BatchCandidateDto>((item) => ({
          item: { ...item },
          outsideBatchRange:
            item.invoiceDate !== null &&
            (item.invoiceDate < batch.startDate || item.invoiceDate > batch.endDate),
          eligible: !["recognition_failed", "suspected_duplicate"].includes(item.status),
          disabledReason:
            item.status === "recognition_failed"
              ? "recognition_failed"
              : item.status === "suspected_duplicate"
                ? "suspected_duplicate"
                : null,
        }));
      return page(candidates);
    }],
    ["create_month_batch", (arguments_) => {
      const year = Number(arguments_.year);
      const month = Number(arguments_.month);
      const startDate = `${year}-${String(month).padStart(2, "0")}-01`;
      const endDate = new Date(Date.UTC(year, month, 0)).toISOString().slice(0, 10);
      return createBatch(state, { name: `${year} 年 ${month} 月报销`, startDate, endDate, note: null });
    }],
    ["create_custom_batch", (arguments_) =>
      createBatch(state, requiredObject<NewBatchInputDto>(arguments_, "input"))],
    ["assign_items_to_batch", (arguments_) => {
      const batchId = requiredString(arguments_, "batchId");
      batchDetail(state, batchId);
      const itemIds = arguments_.itemIds;
      if (!Array.isArray(itemIds) || !itemIds.every((id) => typeof id === "string")) {
        throw new Error("Browser command argument itemIds must be a string array");
      }
      state.items.forEach((item) => {
        if (itemIds.includes(item.id)) item.batchId = batchId;
      });
      return batchDetail(state, batchId);
    }],
    ["remove_item_from_batch", (arguments_) => {
      const batchId = requiredString(arguments_, "batchId");
      const item = state.items.find(
        (candidate) => candidate.id === requiredString(arguments_, "itemId"),
      );
      if (!item) throw notFound("item");
      if (item.batchId === batchId) item.batchId = null;
      return batchDetail(state, batchId);
    }],
    ["export_batch", (arguments_) => {
      const detail = batchDetail(state, requiredString(arguments_, "batchId"));
      if (detail.summary.unconfirmedCount > 0) {
        throw { code: "conflict", message: "Batch contains unconfirmed items" } satisfies AppError;
      }
      const batch = state.batches.find((candidate) => candidate.id === detail.batch.id);
      if (!batch) throw notFound("batch");
      Object.assign(batch, { status: "exported", lastExportedAt: now, updatedAt: now });
      return {
        directory: `${state.preferences.exportDirectory}/${detail.batch.startDate.slice(0, 7)}`,
        itemCount: detail.summary.itemCount,
        totalAmountCents: detail.summary.totalAmountCents,
      } satisfies ExportResultDto;
    }],
    ["list_mailbox_accounts", () => state.accounts.map((account) => ({ ...account }))],
    ["save_mailbox_account", (arguments_) => {
      const input = requiredObject<SaveMailboxAccountInputDto>(arguments_, "input");
      const account = input.id
        ? state.accounts.find((candidate) => candidate.id === input.id)
        : undefined;
      const saved: MailboxAccountDto = {
        id: account?.id ?? `browser-account-${state.nextAccountId++}`,
        provider: input.provider,
        email: input.email,
        imapHost: input.imapHost ?? (input.provider === "gmail" ? "imap.gmail.com" : "imap.qq.com"),
        imapPort: input.imapPort ?? 993,
        enabled: input.enabled,
        syncIntervalMinutes: input.syncIntervalMinutes,
        lastSyncedAt: null,
        lastError: null,
        lastErrorKind: null,
        lastErrorAt: null,
      };
      if (account) Object.assign(account, saved);
      else state.accounts.push(saved);
      return { ...saved };
    }],
    ["test_mailbox_account", () => undefined],
    ["delete_mailbox_account", (arguments_) => {
      const accountId = requiredString(arguments_, "accountId");
      state.accounts = state.accounts.filter((account) => account.id !== accountId);
    }],
    ["get_preferences", () => ({ ...state.preferences })],
    ["save_preferences", (arguments_) => {
      state.preferences = { ...requiredObject<PreferencesDto>(arguments_, "input") };
      state.storage.exportDirectory = state.preferences.exportDirectory;
      return { ...state.preferences };
    }],
    ["get_storage_status", () => ({ ...state.storage })],
    ["retry_export_recovery", () => ({ ...state.storage, recoveryError: null })],
    ["sync_account_now", (arguments_) => {
      const account = state.accounts.find(
        (candidate) => candidate.id === requiredString(arguments_, "accountId"),
      );
      if (!account) throw notFound("account");
      account.lastSyncedAt = now;
    }],
  ];
  return new Map(handlers);
}

function createBatch(state: BridgeState, input: NewBatchInputDto): BatchDto {
  const batch: BatchDto = {
    id: `browser-batch-${state.nextBatchId++}`,
    name: input.name,
    startDate: input.startDate,
    endDate: input.endDate,
    status: "draft",
    itemCount: 0,
    totalAmountCents: 0,
    unconfirmedCount: 0,
    note: input.note,
    createdAt: now,
    updatedAt: now,
    lastExportedAt: null,
  };
  state.batches.unshift(batch);
  return { ...batch };
}

export function createBrowserCommandBridge(
  options: BrowserBridgeOptions = {},
): BrowserCommandBridge {
  const handlers = makeHandlers(initialState());
  return async <T>(command: string, arguments_: CommandArguments = {}) => {
    const handler = handlers.get(command);
    if (!handler) throw new Error(`Unsupported browser command: ${command}`);
    if (options.delayMs && options.delayMs > 0) {
      await new Promise((resolve) => setTimeout(resolve, options.delayMs));
    }
    if (options.failCommands?.has(command)) {
      throw {
        code: "external",
        service: "browser_bridge",
        retryable: true,
        message: `Simulated browser command failure: ${command}`,
      } satisfies AppError;
    }
    return (await handler(arguments_)) as T;
  };
}

export function browserCommandNames() {
  return [...makeHandlers(initialState()).keys()].sort();
}

export function installBrowserCommandBridge() {
  const search = new URLSearchParams(window.location.search);
  const requestedDelay = Number(search.get("bridgeDelay") ?? 0);
  const delayMs = Number.isFinite(requestedDelay)
    ? Math.min(Math.max(requestedDelay, 0), 2_000)
    : 0;
  const failCommands = new Set(
    (search.get("bridgeError") ?? "").split(",").filter(Boolean),
  );
  window.__INVOICE_COMMAND_BRIDGE__ = createBrowserCommandBridge({
    delayMs,
    failCommands,
  });
}
