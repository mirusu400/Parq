// `%LOCALAPPDATA%\Parq\transactions\*.json` 의 트랜잭션 이력을 표시하는 read-only 패널.
// 시작 시간 내림차순. 각 로그를 펼쳐서 step 별 진행 상태와 오류 detail 까지 확인 가능.

import { useEffect, useState } from "react";
import type { StepStatus, TransactionLog, TransactionStep } from "../types";
import { listTransactions } from "../lib/transactions";

interface Props {
  /** 디스크 작업이 일어났을 때 부모가 증가시키는 카운터 — 변경되면 다시 fetch. */
  refreshKey: number;
}

type LoadState =
  | { kind: "loading" }
  | { kind: "ready"; logs: TransactionLog[] }
  | { kind: "error"; message: string };

function nanosToLocaleString(nanos: number): string {
  // u128 nanos 가 JS number 정밀도 (53비트) 를 넘기진 않는다 — 2^53 ns ≈ 285년 후.
  const ms = Math.floor(nanos / 1_000_000);
  return new Date(ms).toLocaleString();
}

function durationMs(start: number, end: number | null): string {
  if (end === null) return "—";
  const ms = (end - start) / 1_000_000;
  if (ms < 1000) return `${ms.toFixed(0)} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}

function statusColor(status: StepStatus): string {
  switch (status) {
    case "done":
      return "text-emerald-400";
    case "failed":
      return "text-red-400";
    case "running":
      return "text-amber-400";
    case "pending":
      return "text-neutral-500";
  }
}

function resultBadge(result: string | null) {
  if (result === null) {
    return (
      <span className="rounded border border-amber-700 bg-amber-950/40 px-1.5 py-0.5 text-[11px] text-amber-300">
        진행 중
      </span>
    );
  }
  if (result === "committed") {
    return (
      <span className="rounded border border-emerald-700 bg-emerald-950/40 px-1.5 py-0.5 text-[11px] text-emerald-300">
        committed
      </span>
    );
  }
  if (result.startsWith("failed")) {
    return (
      <span className="rounded border border-red-700 bg-red-950/40 px-1.5 py-0.5 text-[11px] text-red-300">
        failed
      </span>
    );
  }
  if (result.startsWith("rolled_back")) {
    return (
      <span className="rounded border border-red-700 bg-red-950/40 px-1.5 py-0.5 text-[11px] text-red-300">
        legacy rolled back
      </span>
    );
  }
  if (result === "dropped_without_finalize") {
    return (
      <span className="rounded border border-red-800 bg-red-950/60 px-1.5 py-0.5 text-[11px] text-red-300">
        ⚠ 미완료
      </span>
    );
  }
  return (
    <span className="rounded border border-neutral-700 px-1.5 py-0.5 text-[11px] text-neutral-400">
      {result}
    </span>
  );
}

function StepRow({ step }: { step: TransactionStep }) {
  return (
    <li className="flex items-start gap-2 py-1 text-xs">
      <span className={`w-14 shrink-0 font-medium ${statusColor(step.status)}`}>
        {step.status}
      </span>
      <span className="flex-1 font-mono text-neutral-300">{step.name}</span>
      <span className="w-20 shrink-0 text-right text-neutral-500">
        {durationMs(step.started_at_unix_nanos, step.ended_at_unix_nanos)}
      </span>
      {step.detail && (
        <p className="mt-0.5 basis-full pl-14 text-red-300 whitespace-pre-wrap">
          {step.detail}
        </p>
      )}
    </li>
  );
}

function LogRow({ log }: { log: TransactionLog }) {
  const [expanded, setExpanded] = useState(false);
  return (
    <li className="rounded border border-neutral-800 bg-neutral-900/50">
      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        className="flex w-full items-center justify-between gap-3 px-3 py-2 text-left hover:bg-neutral-800/50"
      >
        <span className="flex items-center gap-2 text-sm">
          <span className="font-mono text-neutral-400">
            {expanded ? "▾" : "▸"}
          </span>
          <span className="font-medium text-neutral-200">{log.operation}</span>
          <span className="text-neutral-500">·</span>
          <span className="text-neutral-400">{log.disk_summary}</span>
        </span>
        <span className="flex items-center gap-2">
          {resultBadge(log.result)}
          <span className="text-xs text-neutral-500">
            {nanosToLocaleString(log.started_at_unix_nanos)}
          </span>
        </span>
      </button>
      {expanded && (
        <div className="border-t border-neutral-800 px-3 py-2 space-y-2">
          <div className="text-xs text-neutral-400">
            <p>
              <span className="text-neutral-500">id:</span>{" "}
              <span className="font-mono">{log.id}</span>
            </p>
            <p className="mt-0.5">
              <span className="text-neutral-500">plan:</span> {log.plan_summary}
            </p>
            <p className="mt-0.5">
              <span className="text-neutral-500">총 소요:</span>{" "}
              {durationMs(log.started_at_unix_nanos, log.ended_at_unix_nanos)}
            </p>
          </div>
          <ul className="divide-y divide-neutral-800/60">
            {log.steps.map((step, i) => (
              <StepRow key={i} step={step} />
            ))}
            {log.steps.length === 0 && (
              <li className="py-1 text-xs text-neutral-500">step 없음</li>
            )}
          </ul>
        </div>
      )}
    </li>
  );
}

export default function TransactionLogPanel({ refreshKey }: Props) {
  const [state, setState] = useState<LoadState>({ kind: "loading" });

  useEffect(() => {
    setState({ kind: "loading" });
    let cancelled = false;
    listTransactions()
      .then((logs) => {
        if (!cancelled) setState({ kind: "ready", logs });
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        const message =
          typeof err === "string"
            ? err
            : err instanceof Error
              ? err.message
              : String(err);
        setState({ kind: "error", message });
      });
    return () => {
      cancelled = true;
    };
  }, [refreshKey]);

  return (
    <section className="space-y-3">
      <h2 className="text-sm font-medium uppercase tracking-wide text-neutral-400">
        작업 이력
      </h2>
      {state.kind === "loading" && (
        <p className="text-xs text-neutral-500">불러오는 중...</p>
      )}
      {state.kind === "error" && (
        <p className="rounded border border-red-900 bg-red-950/30 p-2 text-xs text-red-300">
          {state.message}
        </p>
      )}
      {state.kind === "ready" && state.logs.length === 0 && (
        <p className="text-xs text-neutral-500">
          기록된 작업이 없습니다. 첫 destructive 작업이 실행되면 여기에 나타납니다.
        </p>
      )}
      {state.kind === "ready" && state.logs.length > 0 && (
        <ul className="space-y-2">
          {state.logs.map((log) => (
            <LogRow key={log.id} log={log} />
          ))}
        </ul>
      )}
    </section>
  );
}
