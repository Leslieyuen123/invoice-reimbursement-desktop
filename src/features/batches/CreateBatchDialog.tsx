import { useMutation, useQueryClient } from "@tanstack/react-query";
import { CalendarRange, CircleAlert } from "lucide-react";
import { type FormEvent, useEffect, useRef, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";

import { api } from "../../lib/api";
import { queryKeys } from "../../lib/queryKeys";
import type { AppError, BatchDetailDto, BatchDto } from "../../types";
import "./Batches.css";

type BatchMode = "month" | "custom";

function errorMessage(error: unknown) {
  if (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string"
  ) {
    return (error as AppError).message;
  }
  return "批次创建失败";
}

function emptyBatchDetail(batch: BatchDto): BatchDetailDto {
  const emptyCategory = { itemCount: 0, amountCents: 0 };
  return {
    batch,
    items: [],
    summary: {
      itemCount: 0,
      totalAmountCents: 0,
      transport: emptyCategory,
      dining: emptyCategory,
      accommodation: emptyCategory,
      hospitality: emptyCategory,
      unconfirmedCount: 0,
    },
    warnings: [],
    issues: [],
  };
}

export function CreateBatchDialog() {
  const now = new Date();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const viewActive = useRef(true);
  const [searchParams] = useSearchParams();
  const [mode, setMode] = useState<BatchMode>(
    searchParams.get("type") === "custom" ? "custom" : "month",
  );
  const [year, setYear] = useState(String(now.getFullYear()));
  const [month, setMonth] = useState(String(now.getMonth() + 1));
  const [name, setName] = useState("");
  const [startDate, setStartDate] = useState("");
  const [endDate, setEndDate] = useState("");
  const [note, setNote] = useState("");
  const [automate, setAutomate] = useState(true);
  useEffect(() => {
    viewActive.current = true;
    return () => {
      viewActive.current = false;
    };
  }, []);
  const createMutation = useMutation({
    mutationFn: () =>
      mode === "month"
        ? api.createMonthBatch(Number(year), Number(month))
        : api.createCustomBatch({
            name,
            startDate,
            endDate,
            note: note.trim() || null,
          }),
    onSuccess: (batch: BatchDto) => {
      queryClient.setQueryData(queryKeys.batch(batch.id), emptyBatchDetail(batch));
      void queryClient.invalidateQueries({ queryKey: queryKeys.batchLists });
      void queryClient.invalidateQueries({ queryKey: queryKeys.dashboard });
      if (viewActive.current) {
        navigate(`/batches/${batch.id}`, {
          replace: true,
          ...(automate ? { state: { runAutomation: true } } : {}),
        });
      }
    },
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    createMutation.mutate();
  }

  return (
    <section className="batch-create-page" aria-labelledby="create-batch-title">
      <header className="batch-page-heading">
        <div>
          <h1 id="create-batch-title">新建批次</h1>
          <p>按自然月整理，或设置精确的起止日期。</p>
        </div>
      </header>

      <form className="batch-create-form" onSubmit={submit}>
        <fieldset className="batch-mode-segmented">
          <legend className="sr-only">批次日期方式</legend>
          {([
            ["month", "按月"],
            ["custom", "自定义范围"],
          ] as const).map(([value, label]) => (
            <label key={value}>
              <input
                type="radio"
                name="batch-mode"
                value={value}
                checked={mode === value}
                onChange={() => setMode(value)}
              />
              <span>{label}</span>
            </label>
          ))}
        </fieldset>

        {mode === "month" ? (
          <div className="batch-form-grid">
            <label className="batch-field">
              <span>年份</span>
              <input
                type="number"
                min="1"
                max="9999"
                required
                value={year}
                onChange={(event) => setYear(event.target.value)}
              />
            </label>
            <label className="batch-field">
              <span>月份</span>
              <select
                value={month}
                onChange={(event) => setMonth(event.target.value)}
              >
                {Array.from({ length: 12 }, (_, index) => (
                  <option value={String(index + 1)} key={index + 1}>
                    {index + 1} 月
                  </option>
                ))}
              </select>
            </label>
          </div>
        ) : (
          <div className="batch-custom-fields">
            <label className="batch-field batch-field-wide">
              <span>批次名称</span>
              <input
                required
                value={name}
                onChange={(event) => setName(event.target.value)}
              />
            </label>
            <div className="batch-form-grid">
              <label className="batch-field">
                <span>开始日期</span>
                <input
                  type="date"
                  required
                  value={startDate}
                  onChange={(event) => setStartDate(event.target.value)}
                />
              </label>
              <label className="batch-field">
                <span>结束日期</span>
                <input
                  type="date"
                  required
                  value={endDate}
                  onChange={(event) => setEndDate(event.target.value)}
                />
              </label>
            </div>
            <label className="batch-field batch-field-wide">
              <span>备注（可选）</span>
              <textarea
                rows={3}
                value={note}
                onChange={(event) => setNote(event.target.value)}
              />
            </label>
          </div>
        )}

        {createMutation.isError ? (
          <div className="batch-inline-error" role="alert">
            <CircleAlert size={16} strokeWidth={1.7} aria-hidden="true" />
            {errorMessage(createMutation.error)}
          </div>
        ) : null}

        <label className="batch-automation-option">
          <input
            type="checkbox"
            aria-label="创建后自动处理并导出"
            checked={automate}
            disabled={createMutation.isPending}
            onChange={(event) => setAutomate(event.target.checked)}
          />
          <span>
            <strong>创建后自动处理并导出</strong>
            <small>同步邮箱、纳入安全票据并生成报销包</small>
          </span>
        </label>

        <div className="batch-form-actions">
          <button
            type="button"
            className="button button-secondary"
            onClick={() => navigate("/batches")}
          >
            取消
          </button>
          <button
            type="submit"
            className="button button-primary"
            disabled={createMutation.isPending}
          >
            <CalendarRange size={16} strokeWidth={1.7} aria-hidden="true" />
            {createMutation.isPending
              ? "正在创建"
              : automate
                ? "创建并自动处理"
                : "创建批次"}
          </button>
        </div>
      </form>
    </section>
  );
}
