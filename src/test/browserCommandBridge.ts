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
  PageRequestDto,
  PreferencesDto,
  ReviewItemInputDto,
  SaveMailboxAccountInputDto,
  StorageStatusDto,
  TestMailboxAccountInputDto,
} from "../types";
import { API_COMMANDS } from "../lib/api";
import { isRecentItem } from "../lib/recent";

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
  seed?: BrowserBridgeSeed;
  persist?: (state: BridgeState) => void;
}

export interface BrowserBridgeSeed {
  items?: InvoiceItemDto[];
  batches?: BatchDto[];
  accounts?: MailboxAccountDto[];
  preferences?: PreferencesDto;
  storage?: StorageStatusDto;
  nextItemId?: number;
  nextBatchId?: number;
  nextAccountId?: number;
}

const now = "2026-07-17T08:00:00Z";
const previewFixtureUrl = "/src-tauri/tests/fixtures/image-invoice.png";
const sessionStorageKey = "invoice-reimbursement.browser-bridge.v1";

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

function normalizeCandidateQuery(value: unknown) {
  const query =
    typeof value === "string"
      ? value.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, "")
      : "";
  if (Array.from(query).length > 200 || /\p{Cc}/u.test(query)) {
    throw {
      code: "validation",
      field: "query",
      message: "query must contain at most 200 printable characters",
    } satisfies AppError;
  }
  return query.toLocaleLowerCase("zh-CN");
}

function paginate<T>(
  values: T[],
  request: unknown,
  cursorFor: (value: T) => { sortValue: string; id: string },
): PageDto<T> {
  const pageRequest = (request ?? {}) as PageRequestDto;
  const pageSize = pageRequest.pageSize ?? 50;
  if (!Number.isInteger(pageSize) || pageSize < 1 || pageSize > 200) {
    throw {
      code: "validation",
      field: "pageSize",
      message: "page size must be between 1 and 200",
    } satisfies AppError;
  }
  const ordered = [...values].sort((left, right) => {
    const leftCursor = cursorFor(left);
    const rightCursor = cursorFor(right);
    return (
      rightCursor.sortValue.localeCompare(leftCursor.sortValue) ||
      rightCursor.id.localeCompare(leftCursor.id)
    );
  });
  const bounded = pageRequest.cursor
    ? ordered.filter((value) => {
        const cursor = cursorFor(value);
        return (
          cursor.sortValue < pageRequest.cursor!.sortValue ||
          (cursor.sortValue === pageRequest.cursor!.sortValue &&
            cursor.id < pageRequest.cursor!.id)
        );
      })
    : ordered;
  const items = bounded.slice(0, pageSize);
  return {
    items,
    nextCursor:
      bounded.length > pageSize && items.length > 0
        ? cursorFor(items[items.length - 1])
        : null,
  };
}

function initialState(seed: BrowserBridgeSeed = {}): BridgeState {
  const defaults: BridgeState = {
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
  return {
    ...defaults,
    ...seed,
    items: seed.items?.map((item) => ({ ...item })) ?? defaults.items,
    batches: seed.batches?.map((batch) => ({ ...batch })) ?? defaults.batches,
    accounts: seed.accounts?.map((account) => ({ ...account })) ?? defaults.accounts,
    preferences: { ...(seed.preferences ?? defaults.preferences) },
    storage: { ...(seed.storage ?? defaults.storage) },
  };
}

function markBatchDraft(state: BridgeState, batchId: string | null) {
  if (!batchId) return;
  const batch = state.batches.find((candidate) => candidate.id === batchId);
  if (batch?.status === "exported") {
    Object.assign(batch, { status: "draft", updatedAt: now });
  }
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
    (!filter.recent || isRecentItem(item.createdAt, Date.parse(now))) &&
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
    recentlyAddedCount: state.items.filter((item) =>
      isRecentItem(item.createdAt, Date.parse(now)),
    ).length,
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
    [API_COMMANDS.getDashboard, () => dashboard(state)],
    [API_COMMANDS.listItems, (arguments_) => {
      const filter = (arguments_.filter ?? {}) as ItemFilter;
      return paginate(
        state.items
          .filter((item) => itemMatches(item, filter))
          .map((item) => ({ ...item })),
        arguments_.page,
        (item) => ({ sortValue: item.createdAt, id: item.id }),
      );
    }],
    [API_COMMANDS.getItem, (arguments_) => {
      const item = state.items.find(
        (candidate) => candidate.id === requiredString(arguments_, "itemId"),
      );
      if (!item) throw notFound("item");
      return { ...item };
    }],
    [API_COMMANDS.importManualFiles, (arguments_) => {
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
    [API_COMMANDS.reviewItem, (arguments_) => {
      const input = requiredObject<ReviewItemInputDto>(arguments_, "input");
      const item = state.items.find((candidate) => candidate.id === input.id);
      if (!item) throw notFound("item");
      markBatchDraft(state, item.batchId);
      Object.assign(item, input, {
        confirmationStatus: "confirmed",
        recognitionStatus: "succeeded",
        status: "ready",
        updatedAt: now,
      });
      return { ...item };
    }],
    [API_COMMANDS.resolveDuplicate, (arguments_) => {
      const itemId = requiredString(arguments_, "itemId");
      const item = state.items.find((candidate) => candidate.id === itemId);
      if (!item) throw notFound("item");
      markBatchDraft(state, item.batchId);
      if (arguments_.keep === false) {
        state.items = state.items.filter((candidate) => candidate.id !== itemId);
        return null;
      }
      Object.assign(item, { dedupeStatus: "resolved", status: "pending_confirmation", updatedAt: now });
      return { ...item };
    }],
    [API_COMMANDS.retryRecognition, (arguments_) => {
      const item = state.items.find(
        (candidate) => candidate.id === requiredString(arguments_, "itemId"),
      );
      if (!item) throw notFound("item");
      markBatchDraft(state, item.batchId);
      Object.assign(item, { recognitionStatus: "succeeded", status: "pending_confirmation", updatedAt: now });
      return { ...item };
    }],
    [API_COMMANDS.listBatches, (arguments_) =>
      paginate(
        state.batches.map((batch) => ({ ...batch })),
        arguments_.page,
        (batch) => ({ sortValue: batch.updatedAt, id: batch.id }),
      )],
    [API_COMMANDS.getBatch, (arguments_) =>
      batchDetail(state, requiredString(arguments_, "batchId"))],
    [API_COMMANDS.listBatchCandidates, (arguments_) => {
      const batchId = requiredString(arguments_, "batchId");
      const batch = state.batches.find((candidate) => candidate.id === batchId);
      if (!batch) throw notFound("batch");
      const query = normalizeCandidateQuery(arguments_.query);
      const candidates = state.items
        .filter((item) => {
          if (item.batchId !== null) return false;
          if (query) {
            return [item.originalName, item.company, item.city, item.note].some(
              (value) =>
                value?.toLocaleLowerCase("zh-CN").includes(query) ?? false,
            );
          }
          return (
            item.invoiceDate !== null &&
            item.invoiceDate >= batch.startDate &&
            item.invoiceDate <= batch.endDate
          );
        })
        .map<BatchCandidateDto>((item) => ({
          item: { ...item },
          outsideBatchRange:
            item.invoiceDate === null ||
            item.invoiceDate < batch.startDate ||
            item.invoiceDate > batch.endDate,
          eligible:
            item.dedupeStatus !== "suspected_duplicate" &&
            item.recognitionStatus !== "failed",
          disabledReason:
            item.dedupeStatus === "suspected_duplicate"
              ? "suspected_duplicate"
              : item.recognitionStatus === "failed"
                ? "recognition_failed"
                : null,
        }));
      return paginate(candidates, arguments_.page, (candidate) => ({
        sortValue: candidate.item.createdAt,
        id: candidate.item.id,
      }));
    }],
    [API_COMMANDS.createMonthBatch, (arguments_) => {
      const year = Number(arguments_.year);
      const month = Number(arguments_.month);
      const startDate = `${year}-${String(month).padStart(2, "0")}-01`;
      const endDate = new Date(Date.UTC(year, month, 0)).toISOString().slice(0, 10);
      return createBatch(state, { name: `${year} 年 ${month} 月报销`, startDate, endDate, note: null });
    }],
    [API_COMMANDS.createCustomBatch, (arguments_) =>
      createBatch(state, requiredObject<NewBatchInputDto>(arguments_, "input"))],
    [API_COMMANDS.assignItemsToBatch, (arguments_) => {
      const batchId = requiredString(arguments_, "batchId");
      batchDetail(state, batchId);
      const itemIds = arguments_.itemIds;
      if (!Array.isArray(itemIds) || !itemIds.every((id) => typeof id === "string")) {
        throw new Error("Browser command argument itemIds must be a string array");
      }
      const items = itemIds.map((itemId) => {
        const item = state.items.find((candidate) => candidate.id === itemId);
        if (!item) throw notFound("item");
        if (
          item.dedupeStatus === "suspected_duplicate" ||
          item.recognitionStatus === "failed"
        ) {
          throw {
            code: "conflict",
            message: `Item ${itemId} cannot be assigned to a batch`,
          } satisfies AppError;
        }
        return item;
      });
      markBatchDraft(state, batchId);
      for (const item of items) {
        markBatchDraft(state, item.batchId);
        Object.assign(item, { batchId, updatedAt: now });
      }
      return batchDetail(state, batchId);
    }],
    [API_COMMANDS.removeItemFromBatch, (arguments_) => {
      const batchId = requiredString(arguments_, "batchId");
      batchDetail(state, batchId);
      const item = state.items.find(
        (candidate) => candidate.id === requiredString(arguments_, "itemId"),
      );
      if (!item) throw notFound("item");
      if (item.batchId !== batchId) {
        throw {
          code: "conflict",
          message: "Item does not belong to the batch",
        } satisfies AppError;
      }
      markBatchDraft(state, batchId);
      Object.assign(item, { batchId: null, updatedAt: now });
      return batchDetail(state, batchId);
    }],
    [API_COMMANDS.exportBatch, (arguments_) => {
      const detail = batchDetail(state, requiredString(arguments_, "batchId"));
      if (detail.items.length === 0) {
        throw { code: "conflict", message: "Batch is empty" } satisfies AppError;
      }
      const blocked = detail.items.find((item) =>
        [
          "pending_recognition",
          "pending_confirmation",
          "recognition_failed",
          "suspected_duplicate",
        ].includes(item.status),
      );
      if (blocked) {
        throw {
          code: "conflict",
          message: `Batch contains blocked item ${blocked.id}`,
        } satisfies AppError;
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
    [API_COMMANDS.listMailboxAccounts, () =>
      state.accounts.map((account) => ({ ...account }))],
    [API_COMMANDS.saveMailboxAccount, (arguments_) => {
      const input = requiredObject<SaveMailboxAccountInputDto>(arguments_, "input");
      const account = input.id
        ? state.accounts.find((candidate) => candidate.id === input.id)
        : undefined;
      if (input.id && !account) throw notFound("account");
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
    [API_COMMANDS.testMailboxAccount, (arguments_) => {
      const input = requiredObject<TestMailboxAccountInputDto>(arguments_, "input");
      if (
        input.id &&
        !state.accounts.some((account) => account.id === input.id)
      ) {
        throw notFound("account");
      }
    }],
    [API_COMMANDS.deleteMailboxAccount, (arguments_) => {
      const accountId = requiredString(arguments_, "accountId");
      if (!state.accounts.some((account) => account.id === accountId)) {
        throw notFound("account");
      }
      state.accounts = state.accounts.filter((account) => account.id !== accountId);
    }],
    [API_COMMANDS.getPreferences, () => ({ ...state.preferences })],
    [API_COMMANDS.savePreferences, (arguments_) => {
      state.preferences = { ...requiredObject<PreferencesDto>(arguments_, "input") };
      state.storage.exportDirectory = state.preferences.exportDirectory;
      return { ...state.preferences };
    }],
    [API_COMMANDS.getStorageStatus, () => ({ ...state.storage })],
    [API_COMMANDS.retryExportRecovery, () => {
      state.storage.recoveryError = null;
      return { ...state.storage };
    }],
    [API_COMMANDS.syncAccountNow, (arguments_) => {
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
  const state = initialState(options.seed);
  const handlers = makeHandlers(state);
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
    const result = await handler(arguments_);
    options.persist?.(JSON.parse(JSON.stringify(state)) as BridgeState);
    return result as T;
  };
}

export function browserCommandNames() {
  return [...makeHandlers(initialState()).keys()].sort();
}

export function installBrowserCommandBridge() {
  const search = new URLSearchParams(window.location.search);
  if (search.get("bridgeReset") === "1") {
    window.sessionStorage.removeItem(sessionStorageKey);
  }
  const requestedDelay = Number(search.get("bridgeDelay") ?? 0);
  const delayMs = Number.isFinite(requestedDelay)
    ? Math.min(Math.max(requestedDelay, 0), 2_000)
    : 0;
  const failCommands = new Set(
    (search.get("bridgeError") ?? "").split(",").filter(Boolean),
  );
  let seed: BrowserBridgeSeed | undefined;
  const stored = window.sessionStorage.getItem(sessionStorageKey);
  if (stored) {
    try {
      seed = JSON.parse(stored) as BrowserBridgeSeed;
    } catch {
      window.sessionStorage.removeItem(sessionStorageKey);
    }
  }
  window.__INVOICE_COMMAND_BRIDGE__ = createBrowserCommandBridge({
    delayMs,
    failCommands,
    seed,
    persist: (state) => {
      window.sessionStorage.setItem(sessionStorageKey, JSON.stringify(state));
    },
  });
}
