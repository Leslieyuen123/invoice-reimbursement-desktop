import type {
  AppError,
  BatchAutomationResultDto,
  BatchCandidateDto,
  BatchDetailDto,
  BatchDetailSummaryDto,
  BatchDto,
  BatchIssueDto,
  BatchRepairDto,
  Category,
  DashboardDto,
  ExportResultDto,
  InvoiceItemDto,
  ItemFilter,
  ItemStatus,
  MailLedgerCountsDto,
  MailLedgerEntryDto,
  MailLedgerFilter,
  MailLedgerPageDto,
  MailboxAccountDto,
  SettleBatchInputDto,
  SettleBatchOutcomeDto,
  UpdateBatchRangeInputDto,
  ManualImportOutcomeDto,
  NewBatchInputDto,
  PageDto,
  PageRequestDto,
  PreferencesDto,
  ReviewItemInputDto,
  SaveMailboxAccountInputDto,
  ConsistencyReportDto,
  SyncProgressDto,
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
  mailLedger: MailLedgerEntryDto[];
  batches: BatchDto[];
  accounts: MailboxAccountDto[];
  preferences: PreferencesDto;
  storage: StorageStatusDto;
  consistency: ConsistencyReportDto;
  syncProgress: SyncProgressDto[];
  nextItemId: number;
  nextBatchId: number;
  nextAccountId: number;
}

type CommandHandler = (arguments_: CommandArguments) => unknown | Promise<unknown>;

interface BrowserBridgeOptions {
  delayMs?: number;
  failCommands?: ReadonlySet<string>;
  failOnceCommands?: ReadonlySet<string>;
  automationScenario?: "success-with-exceptions";
  seed?: BrowserBridgeSeed;
  persist?: (state: BridgeState) => void;
}

export interface BrowserBridgeSeed {
  items?: InvoiceItemDto[];
  mailLedger?: MailLedgerEntryDto[];
  batches?: BatchDto[];
  accounts?: MailboxAccountDto[];
  preferences?: PreferencesDto;
  storage?: StorageStatusDto;
  consistency?: ConsistencyReportDto;
  syncProgress?: SyncProgressDto[];
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
    mailLedger: [],
    batches: [],
    accounts: [],
    preferences: {
      backgroundSyncEnabled: true,
      exportDirectory: "/tmp/invoice-reimbursement/e2e",
      batchDirectoryPattern: "{batchName}-{timestamp}",
      markProcessedMailSeen: true,
    },
    storage: {
      localDataDirectory: "/tmp/invoice-reimbursement/e2e/data",
      exportDirectory: "/tmp/invoice-reimbursement/e2e",
      availableBytes: 8_589_934_592,
      recoveryError: null,
    },
    consistency: {
      checkedAt: now,
      itemsChecked: 0,
      batchesChecked: 0,
      issues: [],
    },
    syncProgress: [],
    nextItemId: 1,
    nextBatchId: 1,
    nextAccountId: 1,
  };
  return {
    ...defaults,
    ...seed,
    items: seed.items?.map((item) => ({ ...item })) ?? defaults.items,
    mailLedger:
      seed.mailLedger?.map((entry) => ({ ...entry })) ?? defaults.mailLedger,
    batches: seed.batches?.map((batch) => ({ ...batch })) ?? defaults.batches,
    accounts: seed.accounts?.map((account) => ({ ...account })) ?? defaults.accounts,
    preferences: { ...(seed.preferences ?? defaults.preferences) },
    storage: { ...(seed.storage ?? defaults.storage) },
    consistency: { ...(seed.consistency ?? defaults.consistency) },
    syncProgress: seed.syncProgress?.map((entry) => ({ ...entry })) ?? defaults.syncProgress,
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
  const issues = items
    .map((item) => exportBlocker(item))
    .filter((issue): issue is BatchIssueDto => issue !== null);
  return {
    batch: { ...batch },
    items: items.map((item) => ({ ...item })),
    summary,
    warnings: [],
    issues,
  };
}

/**
 * Mirrors `derive_item_status` in `src-tauri/src/domain/model.rs`: the backend
 * derives the item status from the three status columns instead of trusting a
 * stored field, so the bridge must do the same.
 */
function derivedStatus(item: InvoiceItemDto): ItemStatus {
  if (item.dedupeStatus === "suspected_duplicate") return "suspected_duplicate";
  if (item.recognitionStatus === "failed") return "recognition_failed";
  if (item.recognitionStatus === "pending") return "pending_recognition";
  if (item.confirmationStatus === "pending") return "pending_confirmation";
  return "ready";
}

/**
 * Mirrors the backend export preflight in
 * `src-tauri/src/services/batch_eligibility.rs`, so the development bridge
 * rejects the same batches the real app rejects.
 */
function exportBlocker(item: InvoiceItemDto): BatchIssueDto | null {
  const issue = (code: string, message: string, repairable = false): BatchIssueDto => ({
    itemId: item.id,
    fileName: item.originalName,
    code,
    message,
    repairable,
  });
  const status = derivedStatus(item);
  if (status === "suspected_duplicate") {
    return issue("suspected_duplicate", "票据疑似重复，请先在待处理池处理");
  }
  if (status === "recognition_failed") {
    return issue("recognition_failed", "票据识别失败，请先重新识别或更换原件");
  }
  if (status === "pending_recognition") {
    return issue("not_recognized", "票据尚未完成识别");
  }
  if (status === "pending_confirmation") {
    return issue("not_confirmed", "票据尚未确认");
  }
  if (item.finalCategory === null) return issue("missing_category", "票据分类不能为空");
  if (item.suggestedPeriod === null) {
    return issue("missing_period", "建议归属时间不能为空");
  }
  if (item.amountCents === null) return issue("missing_amount", "票据金额不能为空");
  if (item.currency !== "CNY") return issue("unsupported_currency", "仅支持人民币票据");
  if (!item.hasNormalizedPdf) {
    return issue("missing_normalized_pdf", "票据缺少归一化 PDF", canNormalize(item));
  }
  return null;
}

function canNormalize(item: InvoiceItemDto) {
  return /\.(pdf|jpe?g|png)$/iu.test(item.originalName);
}

/** Mirrors `BatchService::settle_items` for the development bridge. */
function settleBatchItems(
  state: BridgeState,
  batchId: string,
  input: SettleBatchInputDto,
) {
  const detail = batchDetail(state, batchId);
  const skipped: SettleBatchOutcomeDto["skipped"] = [];
  let confirmedCount = 0;
  let filledInvoiceDateCount = 0;
  let appliedCategoryCount = 0;
  for (const item of detail.items) {
    if (item.confirmationStatus === "confirmed") continue;
    const skip = (code: string, message: string) =>
      skipped.push({ itemId: item.id, fileName: item.originalName, code, message });
    if (item.dedupeStatus === "suspected_duplicate") {
      skip("suspected_duplicate", "疑似重复，需先处理重复");
      continue;
    }
    if (item.recognitionStatus === "failed") {
      skip("recognition_failed", "识别失败，需重新识别或更换原件");
      continue;
    }
    if (item.recognitionStatus === "pending") {
      skip("not_recognized", "尚未完成识别");
      continue;
    }
    if (item.amountCents === null) {
      skip("missing_amount", "缺少金额，必须对照原件人工填写");
      continue;
    }
    const derivedDate =
      item.invoiceDate ??
      (input.fillInvoiceDateFromReceived && item.sourceType === "email"
        ? mailReceivedDate(item)
        : null);
    if (derivedDate === null) {
      skip("missing_invoice_date", "缺少开票日期，且无法从邮件日期推导");
      continue;
    }
    const period = item.suggestedPeriod ?? derivedDate.slice(0, 7);
    const category =
      item.finalCategory ??
      (input.applySuggestedCategory ? item.suggestedCategory : null) ??
      input.defaultCategory;
    if (category === null) {
      skip("missing_category", "缺少分类，且没有可采用的建议分类");
      continue;
    }
    if (!item.hasNormalizedPdf && !/\\.(pdf|jpe?g|png)$/iu.test(item.originalName)) {
      skip(
        "unsupported_format",
        "原件格式不支持生成归一化 PDF，请改用 PDF/JPG/PNG 或移出批次",
      );
      continue;
    }
    // Mirrors `batch_membership_date`: email invoices belong to the mail month,
    // manual uploads to their invoice date.
    const membershipDate =
      item.sourceType === "email" ? mailReceivedDate(item) : derivedDate;
    if (
      membershipDate < detail.batch.startDate ||
      membershipDate > detail.batch.endDate
    ) {
      skip("outside_date_range", "票据日期不在批次范围内");
      continue;
    }
    const target = state.items.find((candidate) => candidate.id === item.id);
    if (!target) continue;
    if (item.invoiceDate === null) filledInvoiceDateCount += 1;
    if (item.finalCategory !== category) appliedCategoryCount += 1;
    Object.assign(target, {
      invoiceDate: derivedDate,
      suggestedPeriod: period,
      finalCategory: category,
      recognitionStatus: "succeeded",
      confirmationStatus: "confirmed",
      updatedAt: now,
    });
    // The backend derives `status` from the three status columns.
    target.status = derivedStatus(target);
    confirmedCount += 1;
  }
  if (confirmedCount !== 0) markBatchDraft(state, batchId);
  const repairedCount = repairMissingNormalized(state, batchDetail(state, batchId).items);
  const refreshed = batchDetail(state, batchId);
  return {
    confirmedCount,
    filledInvoiceDateCount,
    appliedCategoryCount,
    repairedCount,
    skipped,
    issues: refreshed.issues,
  } satisfies SettleBatchOutcomeDto;
}

/** The received date is the invoice date fallback for email invoices. */
function mailReceivedDate(item: InvoiceItemDto) {
  return item.fetchedAt.slice(0, 10);
}

/** Mirrors `repair_missing_normalized_pdfs` for the development bridge. */
function repairMissingNormalized(state: BridgeState, items: InvoiceItemDto[]) {
  let repairedCount = 0;
  for (const item of items) {
    const issue = exportBlocker(item);
    if (issue?.code !== "missing_normalized_pdf" || !issue.repairable) continue;
    const target = state.items.find((candidate) => candidate.id === item.id);
    if (!target) continue;
    Object.assign(target, { hasNormalizedPdf: true, updatedAt: now });
    repairedCount += 1;
  }
  return repairedCount;
}

function describeIssues(issues: BatchIssueDto[]) {
  const named = issues
    .slice(0, 3)
    .map((issue) => `${issue.fileName}（${issue.message}）`)
    .join("；");
  return issues.length > 3
    ? `${named}；另有 ${issues.length - 3} 张同类问题票据`
    : named;
}

function exportBatch(state: BridgeState, batchId: string): ExportResultDto {
  const detail = batchDetail(state, batchId);
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
  if (detail.issues.length !== 0) {
    throw {
      code: "conflict",
      message: `${detail.issues.length} 张票据无法导出：${describeIssues(detail.issues)}`,
    } satisfies AppError;
  }
  const batch = state.batches.find((candidate) => candidate.id === batchId);
  if (!batch) throw notFound("batch");
  Object.assign(batch, { status: "exported", lastExportedAt: now, updatedAt: now });
  return {
    directory: `${state.preferences.exportDirectory}/${detail.batch.startDate.slice(0, 7)}`,
    itemCount: detail.summary.itemCount,
    totalAmountCents: detail.summary.totalAmountCents,
  };
}

function isSafeAutomationCandidate(item: InvoiceItemDto, batch: BatchDto) {
  return (
    derivedStatus(item) === "ready" &&
    item.invoiceDate !== null &&
    item.invoiceDate >= batch.startDate &&
    item.invoiceDate <= batch.endDate &&
    item.dedupeStatus !== "suspected_duplicate" &&
    item.finalCategory !== null &&
    item.amountCents !== null &&
    Number.isSafeInteger(item.amountCents) &&
    item.amountCents >= 0 &&
    item.currency === "CNY" &&
    item.suggestedPeriod !== null &&
    item.hasNormalizedPdf
  );
}

function seedAutomationScenario(
  state: BridgeState,
  batch: BatchDto,
  accountId: string,
  scenario: BrowserBridgeOptions["automationScenario"],
) {
  if (scenario !== "success-with-exceptions") return 0;
  const period = batch.startDate.slice(0, 7);
  const fixtures: InvoiceItemDto[] = [
    {
      id: `automation-safe-${batch.id}`,
      originalName: "自动处理安全票据.pdf",
      previewUrl: previewFixtureUrl,
      sourceType: "email",
      sourceAccountId: accountId,
      fetchedAt: now,
      sourceReceivedDate: null,
      invoiceDate: batch.startDate,
      suggestedPeriod: period,
      batchId: null,
      suggestedCategory: "transport",
      finalCategory: "transport",
      amountCents: 8_600,
      currency: "CNY",
      city: "上海",
      company: "自动处理测试交通",
      status: "ready",
      recognitionStatus: "succeeded",
      confirmationStatus: "confirmed",
      dedupeStatus: "unique",
      hasNormalizedPdf: true,
      note: null,
      eventTag: null,
      projectTag: null,
      createdAt: now,
      updatedAt: now,
    },
    {
      id: `automation-exception-${batch.id}`,
      originalName: "自动处理待复核票据.pdf",
      previewUrl: previewFixtureUrl,
      sourceType: "email",
      sourceAccountId: accountId,
      fetchedAt: now,
      sourceReceivedDate: null,
      invoiceDate: batch.startDate,
      suggestedPeriod: period,
      batchId: null,
      suggestedCategory: "dining",
      finalCategory: null,
      amountCents: null,
      currency: "CNY",
      city: null,
      company: null,
      status: "pending_confirmation",
      recognitionStatus: "succeeded",
      confirmationStatus: "pending",
      dedupeStatus: "unique",
      hasNormalizedPdf: true,
      note: null,
      eventTag: null,
      projectTag: null,
      createdAt: now,
      updatedAt: now,
    },
  ];
  const imported = fixtures.filter(
    (fixture) => !state.items.some((item) => item.id === fixture.id),
  );
  state.items.unshift(...imported);
  return imported.length;
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
    sourceReceivedDate: null,
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
    hasNormalizedPdf: true,
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
    mailNeedsAttentionCount: state.mailLedger.filter(
      (entry) => entry.outcome === "partial" || entry.outcome === "failed",
    ).length,
    recentBatches: state.batches.slice(0, 5).map((batch) => ({ ...batch })),
  };
}

function makeHandlers(
  state: BridgeState,
  persistState: () => void = () => undefined,
  automationScenario?: BrowserBridgeOptions["automationScenario"],
): Map<string, CommandHandler> {
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
    [API_COMMANDS.openItemOriginal, (arguments_) => {
      const itemId = requiredString(arguments_, "itemId");
      if (!state.items.some((candidate) => candidate.id === itemId)) {
        throw notFound("item");
      }
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
        .map<BatchCandidateDto>((item) => {
          const disabledReason = exportBlocker(item)?.code ?? null;
          return {
            item: { ...item },
            outsideBatchRange:
              item.invoiceDate === null ||
              item.invoiceDate < batch.startDate ||
              item.invoiceDate > batch.endDate,
            eligible: disabledReason === null,
            disabledReason:
              disabledReason as BatchCandidateDto["disabledReason"],
          };
        });
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
      const blocked = itemIds
        .map((itemId) => {
          const item = state.items.find((candidate) => candidate.id === itemId);
          if (!item) throw notFound("item");
          return exportBlocker(item);
        })
        .filter((issue): issue is BatchIssueDto => issue !== null);
      if (blocked.length !== 0) {
        throw {
          code: "conflict",
          message: `${blocked.length} 张票据无法加入批次：${describeIssues(blocked)}`,
        } satisfies AppError;
      }
      const items = itemIds.map((itemId) => {
        const item = state.items.find((candidate) => candidate.id === itemId);
        if (!item) throw notFound("item");
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
    [API_COMMANDS.exportBatch, (arguments_) =>
      exportBatch(state, requiredString(arguments_, "batchId"))],
    [API_COMMANDS.runBatchAutomation, (arguments_) => {
      const batchId = requiredString(arguments_, "batchId");
      const batch = state.batches.find((candidate) => candidate.id === batchId);
      if (!batch) throw notFound("batch");
      const inclusiveDays =
        (Date.parse(batch.endDate) - Date.parse(batch.startDate)) / 86_400_000 + 1;
      if (inclusiveDays > 366) {
        throw {
          code: "validation",
          field: "dateRange",
          message: "batch automation date range must not exceed 366 days",
        } satisfies AppError;
      }
      const enabledAccounts = state.accounts.filter((account) => account.enabled);
      if (enabledAccounts.length === 0) {
        throw {
          code: "conflict",
          message: "no enabled mailbox accounts are configured",
        } satisfies AppError;
      }
      const importedCount = seedAutomationScenario(
        state,
        batch,
        enabledAccounts[0].id,
        automationScenario,
      );
      const candidates = state.items.filter(
        (item) =>
          item.batchId === null &&
          item.invoiceDate !== null &&
          item.invoiceDate >= batch.startDate &&
          item.invoiceDate <= batch.endDate,
      );
      const safeCandidates = candidates.filter((item) =>
        isSafeAutomationCandidate(item, batch),
      );
      markBatchDraft(state, batchId);
      for (const item of safeCandidates) {
        Object.assign(item, { batchId, updatedAt: now });
      }
      const detail = batchDetail(state, batchId);
      const repairedCount = repairMissingNormalized(state, detail.items);
      if (repairedCount !== 0) markBatchDraft(state, batchId);
      const refreshed = repairedCount === 0 ? detail : batchDetail(state, batchId);
      persistState();
      return {
        scannedAccountCount: enabledAccounts.length,
        failedAccounts: [],
        importedCount,
        assignedCount: safeCandidates.length,
        exceptionCount: candidates.length - safeCandidates.length,
        repairedCount,
        export:
          refreshed.items.length === 0 ? null : exportBatch(state, batchId),
      } satisfies BatchAutomationResultDto;
    }],
    [API_COMMANDS.updateBatchRange, (arguments_) => {
      const batchId = requiredString(arguments_, "batchId");
      const input = requiredObject<UpdateBatchRangeInputDto>(arguments_, "input");
      const batch = state.batches.find((candidate) => candidate.id === batchId);
      if (!batch) throw notFound("batch");
      if (input.startDate > input.endDate) {
        throw {
          code: "validation",
          field: "dateRange",
          message: "start date must not be after end date",
        } satisfies AppError;
      }
      Object.assign(batch, {
        startDate: input.startDate,
        endDate: input.endDate,
        status: "draft",
        updatedAt: now,
      });
      persistState();
      return batchDetail(state, batchId);
    }],
    [API_COMMANDS.settleBatchItems, (arguments_) => {
      const batchId = requiredString(arguments_, "batchId");
      const input = requiredObject<SettleBatchInputDto>(arguments_, "input");
      const outcome = settleBatchItems(state, batchId, {
        fillInvoiceDateFromReceived: input.fillInvoiceDateFromReceived === true,
        applySuggestedCategory: input.applySuggestedCategory === true,
        defaultCategory: input.defaultCategory ?? null,
      });
      persistState();
      return outcome;
    }],
    [API_COMMANDS.removeBatchItems, (arguments_) => {
      const batchId = requiredString(arguments_, "batchId");
      batchDetail(state, batchId);
      const itemIds = arguments_.itemIds;
      if (!Array.isArray(itemIds) || !itemIds.every((id) => typeof id === "string")) {
        throw new Error("Browser command argument itemIds must be a string array");
      }
      markBatchDraft(state, batchId);
      for (const itemId of itemIds) {
        const item = state.items.find((candidate) => candidate.id === itemId);
        if (item?.batchId === batchId) {
          Object.assign(item, { batchId: null, updatedAt: now });
        }
      }
      persistState();
      return batchDetail(state, batchId);
    }],
    [API_COMMANDS.repairBatchNormalizedPdfs, (arguments_) => {
      const batchId = requiredString(arguments_, "batchId");
      const detail = batchDetail(state, batchId);
      const repairedCount = repairMissingNormalized(state, detail.items);
      if (repairedCount !== 0) markBatchDraft(state, batchId);
      persistState();
      return {
        repairedCount,
        issues: batchDetail(state, batchId).issues,
      } satisfies BatchRepairDto;
    }],
    [API_COMMANDS.listMailLedger, (arguments_) => {
      const filter = (arguments_.filter ?? {}) as MailLedgerFilter;
      const query = filter.query?.trim().toLocaleLowerCase("zh-CN");
      const matching = state.mailLedger
        .filter((entry) => {
          if (filter.needsAttention && !["partial", "failed"].includes(entry.outcome)) {
            return false;
          }
          if (filter.outcome && entry.outcome !== filter.outcome) return false;
          if (filter.accountId && entry.accountId !== filter.accountId) return false;
          if (!query) return true;
          return [entry.subject, entry.sender]
            .filter((value): value is string => Boolean(value))
            .some((value) => value.toLocaleLowerCase("zh-CN").includes(query));
        })
        .sort((left, right) => right.receivedAt.localeCompare(left.receivedAt));
      const cursor = (arguments_.page as { cursor?: { receivedAt: string } } | undefined)
        ?.cursor;
      const remaining = cursor
        ? matching.filter((entry) => entry.receivedAt < cursor.receivedAt)
        : matching;
      const pageSize =
        (arguments_.page as { pageSize?: number } | undefined)?.pageSize ?? 50;
      const items = remaining.slice(0, pageSize);
      const last = items.at(-1);
      return {
        items: items.map((entry) => ({ ...entry })),
        nextCursor:
          remaining.length > items.length && last
            ? { receivedAt: last.receivedAt, uid: last.uid, accountId: last.accountId }
            : null,
      } satisfies MailLedgerPageDto;
    }],
    [API_COMMANDS.getMailLedgerCounts, () => {
      const count = (outcome: string) =>
        state.mailLedger.filter((entry) => entry.outcome === outcome).length;
      return {
        imported: count("imported"),
        partial: count("partial"),
        failed: count("failed"),
        ignored: count("ignored"),
        needsAttention: count("partial") + count("failed"),
      } satisfies MailLedgerCountsDto;
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
    [API_COMMANDS.getSyncProgress, () => state.syncProgress.map((entry) => ({ ...entry }))],
    [API_COMMANDS.cancelSync, (arguments_) => {
      const accountId = requiredString(arguments_, "accountId");
      const before = state.syncProgress.length;
      state.syncProgress = state.syncProgress.filter(
        (entry) => entry.accountId !== accountId,
      );
      return state.syncProgress.length !== before;
    }],
    [API_COMMANDS.getConsistencyReport, () => ({
      ...state.consistency,
      itemsChecked: state.items.length,
      batchesChecked: state.batches.length,
    })],
    [API_COMMANDS.exportDiagnostics, () => ({
      path: `${state.storage.exportDirectory}/invoice-diagnostics-20260918-070000.zip`,
      directory: state.storage.exportDirectory,
      bytes: 2048,
    })],
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
  const persistState = () => {
    options.persist?.(JSON.parse(JSON.stringify(state)) as BridgeState);
  };
  const handlers = makeHandlers(state, persistState, options.automationScenario);
  const remainingFailOnceCommands = new Set(options.failOnceCommands);
  return async <T>(command: string, arguments_: CommandArguments = {}) => {
    const handler = handlers.get(command);
    if (!handler) throw new Error(`Unsupported browser command: ${command}`);
    if (options.delayMs && options.delayMs > 0) {
      await new Promise((resolve) => setTimeout(resolve, options.delayMs));
    }
    if (
      options.failCommands?.has(command) ||
      remainingFailOnceCommands.delete(command)
    ) {
      if (command === API_COMMANDS.runBatchAutomation) {
        throw {
          code: "external",
          service: "mailbox",
          retryable: true,
          message: "所有已启用邮箱同步失败，请检查网络和邮箱授权后重试",
        } satisfies AppError;
      }
      throw {
        code: "external",
        service: "browser_bridge",
        retryable: true,
        message: `Simulated browser command failure: ${command}`,
      } satisfies AppError;
    }
    const result = await handler(arguments_);
    persistState();
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
  const failOnceCommands = new Set(
    (search.get("bridgeErrorOnce") ?? "").split(",").filter(Boolean),
  );
  const automationScenario =
    search.get("bridgeAutomation") === "success-with-exceptions"
      ? "success-with-exceptions"
      : undefined;
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
    failOnceCommands,
    automationScenario,
    seed,
    persist: (state) => {
      window.sessionStorage.setItem(sessionStorageKey, JSON.stringify(state));
    },
  });
}
