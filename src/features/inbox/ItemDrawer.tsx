import { Check, RotateCcw, Save, Trash2, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { api } from "../../lib/api";
import { formatAmountCents } from "../../lib/amount";
import {
  MAX_SAFE_AMOUNT_CENTS,
  type AppError,
  type Category,
  type InvoiceItemDto,
} from "../../types";
import { ItemPreview } from "./ItemPreview";

const categories: Array<{ value: Category; label: string }> = [
  { value: "transport", label: "交通" },
  { value: "dining", label: "餐饮" },
  { value: "accommodation", label: "住宿" },
  { value: "hospitality", label: "招待" },
];

const categoryLabels = Object.fromEntries(
  categories.map((category) => [category.value, category.label]),
) as Record<Category, string>;

interface ItemDrawerProps {
  item: InvoiceItemDto;
  onClose: () => void;
  onDeleted: (itemId: string) => void;
  onSaved: (item: InvoiceItemDto) => void;
}

function initialAmount(item: InvoiceItemDto) {
  return item.amountCents === null ? "" : formatAmountCents(item.amountCents);
}

function parseYuanToCents(value: string):
  | { ok: true; cents: number }
  | { ok: false; error: string } {
  const trimmed = value.trim();
  if (!trimmed) return { ok: false, error: "请输入金额" };
  if (/^\d+\.\d{3,}$/.test(trimmed)) {
    return { ok: false, error: "金额最多保留两位小数" };
  }
  const match = /^(\d+)(?:\.(\d{0,2}))?$/.exec(trimmed);
  if (!match) return { ok: false, error: "请输入有效金额" };

  const cents =
    BigInt(match[1]) * 100n + BigInt((match[2] ?? "").padEnd(2, "0"));
  if (cents > BigInt(MAX_SAFE_AMOUNT_CENTS)) {
    return { ok: false, error: "金额超出可保存范围" };
  }
  return { ok: true, cents: Number(cents) };
}

function optional(value: string) {
  const trimmed = value.trim();
  return trimmed ? trimmed : null;
}

function messageFromError(error: unknown) {
  if (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string"
  ) {
    return (error as AppError).message;
  }
  return "保存失败，请重试";
}

export function ItemDrawer({
  item,
  onClose,
  onDeleted,
  onSaved,
}: ItemDrawerProps) {
  const [category, setCategory] = useState<Category>(
    item.finalCategory ?? item.suggestedCategory ?? "transport",
  );
  const [invoiceDate, setInvoiceDate] = useState(item.invoiceDate ?? "");
  const [suggestedPeriod, setSuggestedPeriod] = useState(
    item.suggestedPeriod ?? item.invoiceDate?.slice(0, 7) ?? "",
  );
  const [amount, setAmount] = useState(() => initialAmount(item));
  const [city, setCity] = useState(item.city ?? "");
  const [company, setCompany] = useState(item.company ?? "");
  const [note, setNote] = useState(item.note ?? "");
  const [eventTag, setEventTag] = useState(item.eventTag ?? "");
  const [projectTag, setProjectTag] = useState(item.projectTag ?? "");
  const [amountError, setAmountError] = useState<string | null>(null);
  const [periodError, setPeriodError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [retrying, setRetrying] = useState(false);
  const [resolvingDuplicate, setResolvingDuplicate] = useState<
    "keep" | "delete" | null
  >(null);
  const closeButtonRef = useRef<HTMLButtonElement>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      onCloseRef.current();
    };

    closeButtonRef.current?.focus();
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, []);

  async function save() {
    const parsedAmount = parseYuanToCents(amount);
    if (!parsedAmount.ok) {
      setAmountError(parsedAmount.error);
      return;
    }
    if (!/^\d{4}-\d{2}$/.test(suggestedPeriod)) {
      setPeriodError("请选择建议月份");
      return;
    }
    setAmountError(null);
    setPeriodError(null);
    setSaveError(null);
    setSaving(true);
    try {
      const saved = await api.reviewItem({
        id: item.id,
        invoiceDate: invoiceDate || null,
        suggestedPeriod,
        finalCategory: category,
        amountCents: parsedAmount.cents,
        city: optional(city),
        company: optional(company),
        note: optional(note),
        eventTag: optional(eventTag),
        projectTag: optional(projectTag),
      });
      onSaved(saved);
    } catch (error) {
      setSaveError(messageFromError(error));
    } finally {
      setSaving(false);
    }
  }

  async function retryRecognition() {
    setSaveError(null);
    setRetrying(true);
    try {
      onSaved(await api.retryRecognition(item.id));
    } catch (error) {
      setSaveError(messageFromError(error));
    } finally {
      setRetrying(false);
    }
  }

  async function resolveDuplicate(keep: boolean) {
    setSaveError(null);
    setResolvingDuplicate(keep ? "keep" : "delete");
    try {
      const resolved = await api.resolveDuplicate(item.id, keep);
      if (resolved) onSaved(resolved);
      else onDeleted(item.id);
    } catch (error) {
      setSaveError(messageFromError(error));
    } finally {
      setResolvingDuplicate(null);
    }
  }

  return (
    <aside className="item-drawer" role="dialog" aria-label="票据详情">
      <header className="item-drawer-header">
        <div>
          <span>票据详情</span>
          <h2>{item.originalName}</h2>
        </div>
        <button
          ref={closeButtonRef}
          className="icon-button"
          type="button"
          onClick={onClose}
          aria-label="关闭票据详情"
          title="关闭"
        >
          <X size={18} strokeWidth={1.8} aria-hidden="true" />
        </button>
      </header>

      <div className="item-drawer-body">
        <ItemPreview
          originalName={item.originalName}
          previewUrl={item.previewUrl}
        />

        <div className="item-editor">
          <section className="item-readonly" aria-labelledby="automatic-title">
            <h3 id="automatic-title">自动识别值</h3>
            <dl>
              <div>
                <dt>来源</dt>
                <dd>{item.sourceType === "email" ? "邮箱" : "手动上传"}</dd>
              </div>
              <div>
                <dt>抓取时间</dt>
                <dd>{item.fetchedAt.replace("T", " ").slice(0, 16)}</dd>
              </div>
              <div>
                <dt>识别日期</dt>
                <dd>{item.invoiceDate ?? "未识别"}</dd>
              </div>
              <div>
                <dt>建议月份</dt>
                <dd>{item.suggestedPeriod ?? "待确认"}</dd>
              </div>
              <div>
                <dt>建议分类</dt>
                <dd>
                  {item.suggestedCategory
                    ? categoryLabels[item.suggestedCategory]
                    : "待确认"}
                </dd>
              </div>
              <div>
                <dt>识别金额</dt>
                <dd>
                  {item.amountCents === null
                    ? "未识别"
                    : `¥${formatAmountCents(item.amountCents)}`}
                </dd>
              </div>
            </dl>
          </section>

          <section className="item-form" aria-labelledby="final-values-title">
            <h3 id="final-values-title">最终值</h3>
            <fieldset className="category-segmented">
              <legend>分类</legend>
              <div>
                {categories.map((option) => (
                  <label key={option.value}>
                    <input
                      type="radio"
                      name="category"
                      value={option.value}
                      checked={category === option.value}
                      onChange={() => setCategory(option.value)}
                    />
                    <span>{option.label}</span>
                  </label>
                ))}
              </div>
            </fieldset>

            <div className="form-grid">
              <label>
                <span>开票日期</span>
                <input
                  type="date"
                  value={invoiceDate}
                  onChange={(event) => setInvoiceDate(event.target.value)}
                />
              </label>
              <label>
                <span>建议月份</span>
                <input
                  type="month"
                  value={suggestedPeriod}
                  aria-invalid={periodError ? "true" : undefined}
                  aria-describedby={periodError ? "item-period-error" : undefined}
                  onChange={(event) => {
                    setSuggestedPeriod(event.target.value);
                    setPeriodError(null);
                  }}
                />
                {periodError ? (
                  <span className="field-error" id="item-period-error">
                    {periodError}
                  </span>
                ) : null}
              </label>
              <label>
                <span>金额</span>
                <input
                  value={amount}
                  inputMode="decimal"
                  aria-invalid={amountError ? "true" : undefined}
                  aria-describedby={amountError ? "item-amount-error" : undefined}
                  onChange={(event) => {
                    setAmount(event.target.value);
                    setAmountError(null);
                  }}
                />
                {amountError ? (
                  <span className="field-error" id="item-amount-error">
                    {amountError}
                  </span>
                ) : null}
              </label>
              <label>
                <span>城市</span>
                <input value={city} onChange={(event) => setCity(event.target.value)} />
              </label>
            </div>
            <label>
              <span>主体</span>
              <input
                value={company}
                onChange={(event) => setCompany(event.target.value)}
              />
            </label>
            <label>
              <span>备注</span>
              <textarea
                rows={3}
                value={note}
                onChange={(event) => setNote(event.target.value)}
              />
            </label>
            <div className="form-grid">
              <label>
                <span>事项标签</span>
                <input
                  value={eventTag}
                  onChange={(event) => setEventTag(event.target.value)}
                />
              </label>
              <label>
                <span>项目标签</span>
                <input
                  value={projectTag}
                  onChange={(event) => setProjectTag(event.target.value)}
                />
              </label>
            </div>
          </section>

          <section className="batch-assignment" aria-labelledby="batch-assignment-title">
            <h3 id="batch-assignment-title">批次归属</h3>
            <span>{item.batchId ? `批次 ${item.batchId}` : "未分配批次"}</span>
          </section>
        </div>
      </div>

      <footer className="item-drawer-footer">
        <div role={saveError ? "alert" : undefined}>{saveError}</div>
        {item.dedupeStatus === "suspected_duplicate" ? (
          <div className="duplicate-actions">
            <button
              className="button button-secondary"
              type="button"
              disabled={resolvingDuplicate !== null}
              onClick={() => void resolveDuplicate(true)}
            >
              <Check size={16} strokeWidth={1.7} aria-hidden="true" />
              {resolvingDuplicate === "keep" ? "处理中" : "确认保留"}
            </button>
            <button
              className="button button-danger"
              type="button"
              disabled={resolvingDuplicate !== null}
              onClick={() => void resolveDuplicate(false)}
            >
              <Trash2 size={16} strokeWidth={1.7} aria-hidden="true" />
              {resolvingDuplicate === "delete" ? "删除中" : "删除重复"}
            </button>
          </div>
        ) : null}
        <button
          className="button button-secondary"
          type="button"
          disabled={retrying || saving || resolvingDuplicate !== null}
          onClick={() => void retryRecognition()}
        >
          <RotateCcw size={16} strokeWidth={1.7} aria-hidden="true" />
          {retrying ? "识别中" : "重新识别"}
        </button>
        <button
          className="button button-primary"
          type="button"
          disabled={saving || retrying || resolvingDuplicate !== null}
          onClick={() => void save()}
        >
          <Save size={16} strokeWidth={1.7} aria-hidden="true" />
          {saving ? "保存中" : "保存并确认"}
        </button>
      </footer>
    </aside>
  );
}
