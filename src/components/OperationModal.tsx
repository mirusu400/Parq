// 파괴적 작업의 4단계 (form → plan/preview → confirm → execute) 를 처리하는 모달.
//
// CLAUDE.md 의 위험 작업 처리 원칙을 따라:
// 1. 사용자가 폼에 파라미터 입력
// 2. 백엔드에 plan_* 호출 (read-only 검증)
// 3. plan 요약 + 디스크 모델명 타이핑 확인
// 4. execute_*_dangerous 호출
//
// 라벨 변경 / 파티션 생성 두 가지 작업을 동일한 흐름으로 처리한다.

import { useEffect, useState } from "react";
import type {
  CreatePartitionPlan,
  DeletePartitionPlan,
  DismountPlan,
  Disk,
  FileSystemKind,
  Partition,
  ResizeLimits,
  ResizePartitionPlan,
  SetLabelPlan,
  SizeRequest,
} from "../types";
import {
  executeCreatePartition,
  executeDeletePartition,
  executeDismount,
  executeResizePartition,
  executeSetLabel,
  getResizeLimits,
  planCreatePartition,
  planDeletePartition,
  planDismount,
  planResizePartition,
  planSetLabel,
} from "../lib/operations";
import { formatBytes } from "../lib/format";

export type Operation =
  | { kind: "createPartition"; disk: Disk }
  | { kind: "setLabel"; disk: Disk; partition: Partition }
  | { kind: "deletePartition"; disk: Disk; partition: Partition }
  | { kind: "dismount"; disk: Disk; partition: Partition }
  | {
      kind: "resizePartition";
      disk: Disk;
      partition: Partition;
      /** 드래그-리사이즈에서 미리 채울 초기 newSizeBytes. 모달이 limits 의 [min, max] 로 클램프함. */
      initialSizeBytes?: number;
    };

type PlanResult =
  | CreatePartitionPlan
  | SetLabelPlan
  | DeletePartitionPlan
  | DismountPlan
  | ResizePartitionPlan;

interface Props {
  operation: Operation | null;
  onClose: () => void;
  onSuccess: () => void;
}

type Phase =
  | { stage: "loadingLimits" }
  | { stage: "form" }
  | { stage: "loadingPlan" }
  | {
      stage: "preview";
      plan: PlanResult;
      typed: string;
    }
  | { stage: "loadingExecute"; plan: PlanResult }
  | { stage: "done" }
  | { stage: "error"; message: string };

const FAT32_MAX_LABEL = 11;
const EXFAT_MAX_LABEL = 15;
const NTFS_MAX_LABEL = 32;

function maxLabelLen(fs: FileSystemKind): number {
  if (fs === "FAT32") return FAT32_MAX_LABEL;
  if (fs === "exFAT") return EXFAT_MAX_LABEL;
  return NTFS_MAX_LABEL;
}

function isValidLabelChar(c: string): boolean {
  return /^[a-zA-Z0-9 _-]$/.test(c);
}

function validateLabel(label: string, fs: FileSystemKind): string | null {
  if (label.length > maxLabelLen(fs)) {
    return `라벨이 너무 깁니다 (${fs} 최대 ${maxLabelLen(fs)}자)`;
  }
  if (![...label].every(isValidLabelChar)) {
    return "라벨은 ASCII 영숫자, 공백, '-', '_' 만 허용합니다";
  }
  return null;
}

function errorMessage(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  return String(err);
}

// SI base-10 (Get-Disk / lsblk 관행) — 사용자가 입력한 GB / TB 와 일치시킨다.
type SizeUnit = "MB" | "GB" | "TB";

function unitFactor(u: SizeUnit): number {
  if (u === "MB") return 1_000_000;
  if (u === "GB") return 1_000_000_000;
  return 1_000_000_000_000;
}

export default function OperationModal({
  operation,
  onClose,
  onSuccess,
}: Props) {
  const [phase, setPhase] = useState<Phase>({ stage: "form" });

  // create_partition form 상태
  const [sizeMode, setSizeMode] = useState<"max" | "value">("max");
  const [sizeAmount, setSizeAmount] = useState<string>("");
  const [sizeUnit, setSizeUnit] = useState<SizeUnit>("GB");
  const [fileSystem, setFileSystem] = useState<FileSystemKind>("FAT32");
  const [label, setLabel] = useState<string>("");

  // set_label form 상태
  const [newLabel, setNewLabel] = useState<string>("");

  // resize form 상태
  const [resizeLimits, setResizeLimits] = useState<ResizeLimits | null>(null);
  const [resizeNewBytes, setResizeNewBytes] = useState<number>(0);

  useEffect(() => {
    if (!operation) return;
    setSizeMode("max");
    setSizeAmount("");
    setSizeUnit("GB");
    setFileSystem("FAT32");
    setLabel("");
    setNewLabel(
      operation.kind === "setLabel" ? (operation.partition.label ?? "") : "",
    );
    setResizeLimits(null);
    setResizeNewBytes(0);

    if (operation.kind === "resizePartition") {
      // 리사이즈는 form 표시 전에 먼저 한계를 조회.
      setPhase({ stage: "loadingLimits" });
      let cancelled = false;
      const initial = operation.initialSizeBytes;
      getResizeLimits(operation.disk.id, operation.partition.id)
        .then((limits) => {
          if (cancelled) return;
          setResizeLimits(limits);
          // 드래그가 보낸 초기값이 있으면 limits 로 클램프해서 사용. 없으면 현재 크기.
          const seed =
            initial !== undefined
              ? Math.min(Math.max(initial, limits.minBytes), limits.maxBytes)
              : limits.currentBytes;
          setResizeNewBytes(seed);
          setPhase({ stage: "form" });
        })
        .catch((e: unknown) => {
          if (!cancelled) {
            setPhase({ stage: "error", message: errorMessage(e) });
          }
        });
      return () => {
        cancelled = true;
      };
    }
    setPhase({ stage: "form" });
  }, [operation]);

  if (!operation) return null;

  const handlePlan = async () => {
    setPhase({ stage: "loadingPlan" });
    try {
      if (operation.kind === "deletePartition") {
        const plan = await planDeletePartition(
          operation.disk.id,
          operation.partition.id,
        );
        setPhase({ stage: "preview", plan, typed: "" });
        return;
      }
      if (operation.kind === "dismount") {
        const plan = await planDismount(
          operation.disk.id,
          operation.partition.id,
        );
        setPhase({ stage: "preview", plan, typed: "" });
        return;
      }
      if (operation.kind === "resizePartition") {
        if (!resizeLimits) {
          setPhase({ stage: "error", message: "리사이즈 한계가 로드되지 않았습니다" });
          return;
        }
        if (
          !Number.isFinite(resizeNewBytes) ||
          resizeNewBytes <= 0
        ) {
          setPhase({ stage: "error", message: "유효한 크기를 입력하세요" });
          return;
        }
        const plan = await planResizePartition(
          operation.disk.id,
          operation.partition.id,
          resizeNewBytes,
        );
        setPhase({ stage: "preview", plan, typed: "" });
        return;
      }
      if (operation.kind === "createPartition") {
        const labelErr = label
          ? validateLabel(label, fileSystem)
          : null;
        if (labelErr) {
          setPhase({ stage: "error", message: labelErr });
          return;
        }
        let sizeRequest: SizeRequest;
        if (sizeMode === "max") {
          sizeRequest = { kind: "useMaximum" };
        } else {
          const amount = Number(sizeAmount);
          if (!Number.isFinite(amount) || amount <= 0) {
            setPhase({
              stage: "error",
              message: "크기는 양의 숫자여야 합니다",
            });
            return;
          }
          const bytes = Math.round(amount * unitFactor(sizeUnit));
          sizeRequest = { kind: "bytes", value: bytes };
        }
        const plan = await planCreatePartition(
          operation.disk.id,
          sizeRequest,
          fileSystem,
          label.trim() || null,
        );
        setPhase({ stage: "preview", plan, typed: "" });
      } else {
        const labelErr = validateLabel(
          newLabel.trim(),
          operation.partition.fileSystem,
        );
        if (labelErr) {
          setPhase({ stage: "error", message: labelErr });
          return;
        }
        const plan = await planSetLabel(
          operation.disk.id,
          operation.partition.id,
          newLabel.trim(),
        );
        setPhase({ stage: "preview", plan, typed: "" });
      }
    } catch (e) {
      setPhase({ stage: "error", message: errorMessage(e) });
    }
  };

  const handleExecute = async () => {
    if (phase.stage !== "preview") return;
    if (phase.typed !== operation.disk.model) return;
    const plan = phase.plan;
    setPhase({ stage: "loadingExecute", plan });
    try {
      if (operation.kind === "createPartition") {
        await executeCreatePartition(plan as CreatePartitionPlan);
      } else if (operation.kind === "setLabel") {
        await executeSetLabel(plan as SetLabelPlan);
      } else if (operation.kind === "deletePartition") {
        await executeDeletePartition(plan as DeletePartitionPlan);
      } else if (operation.kind === "dismount") {
        await executeDismount(plan as DismountPlan);
      } else {
        await executeResizePartition(plan as ResizePartitionPlan);
      }
      setPhase({ stage: "done" });
      onSuccess();
    } catch (e) {
      setPhase({ stage: "error", message: errorMessage(e) });
    }
  };

  const title =
    operation.kind === "createPartition"
      ? `파티션 생성 — 디스크 #${operation.disk.number}`
      : operation.kind === "setLabel"
        ? `라벨 변경 — 파티션 #${operation.partition.index}${operation.partition.driveLetter ? ` (${operation.partition.driveLetter}:)` : ""}`
        : operation.kind === "deletePartition"
          ? `파티션 삭제 — 파티션 #${operation.partition.index}`
          : operation.kind === "dismount"
            ? `드라이브 문자 제거 — 파티션 #${operation.partition.index}${operation.partition.driveLetter ? ` (${operation.partition.driveLetter}:)` : ""}`
            : `리사이즈 — 파티션 #${operation.partition.index}${operation.partition.driveLetter ? ` (${operation.partition.driveLetter}:)` : ""}`;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
      role="dialog"
      aria-modal="true"
    >
      <div className="w-full max-w-lg rounded-lg border border-neutral-700 bg-neutral-900 p-6 shadow-2xl">
        <header className="mb-4 flex items-start justify-between gap-4">
          <h2 className="text-lg font-semibold">{title}</h2>
          <button
            type="button"
            onClick={onClose}
            className="rounded px-2 py-0.5 text-sm text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200"
            aria-label="닫기"
          >
            ✕
          </button>
        </header>

        {phase.stage === "loadingLimits" && (
          <p className="text-sm text-neutral-400">리사이즈 한계 조회 중...</p>
        )}

        {phase.stage === "form" && (
          <FormView
            operation={operation}
            sizeMode={sizeMode}
            setSizeMode={setSizeMode}
            sizeAmount={sizeAmount}
            setSizeAmount={setSizeAmount}
            sizeUnit={sizeUnit}
            setSizeUnit={setSizeUnit}
            fileSystem={fileSystem}
            setFileSystem={setFileSystem}
            label={label}
            setLabel={setLabel}
            newLabel={newLabel}
            setNewLabel={setNewLabel}
            resizeLimits={resizeLimits}
            resizeNewBytes={resizeNewBytes}
            setResizeNewBytes={setResizeNewBytes}
            onSubmit={handlePlan}
            onCancel={onClose}
          />
        )}

        {phase.stage === "loadingPlan" && (
          <p className="text-sm text-neutral-400">plan 계산 중...</p>
        )}

        {phase.stage === "preview" && (
          <PreviewView
            disk={operation.disk}
            summary={phase.plan.summary}
            typed={phase.typed}
            onTypedChange={(typed) => setPhase({ ...phase, typed })}
            onExecute={handleExecute}
            onCancel={onClose}
          />
        )}

        {phase.stage === "loadingExecute" && (
          <p className="text-sm text-amber-400">실행 중... 중단하지 마세요.</p>
        )}

        {phase.stage === "done" && (
          <div className="space-y-3">
            <p className="text-sm text-emerald-400">완료. 트랜잭션 commit 됨.</p>
            <button
              type="button"
              onClick={onClose}
              className="rounded bg-neutral-700 px-4 py-1.5 text-sm text-neutral-100 hover:bg-neutral-600"
            >
              닫기
            </button>
          </div>
        )}

        {phase.stage === "error" && (
          <div className="space-y-3">
            <p className="rounded border border-red-900 bg-red-950/40 p-3 text-sm text-red-300 whitespace-pre-wrap">
              {phase.message}
            </p>
            <div className="flex gap-2">
              <button
                type="button"
                onClick={() => setPhase({ stage: "form" })}
                className="rounded bg-neutral-700 px-4 py-1.5 text-sm text-neutral-100 hover:bg-neutral-600"
              >
                다시 시도
              </button>
              <button
                type="button"
                onClick={onClose}
                className="rounded px-4 py-1.5 text-sm text-neutral-400 hover:text-neutral-200"
              >
                닫기
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

interface FormViewProps {
  operation: Operation;
  sizeMode: "max" | "value";
  setSizeMode: (m: "max" | "value") => void;
  sizeAmount: string;
  setSizeAmount: (s: string) => void;
  sizeUnit: SizeUnit;
  setSizeUnit: (u: SizeUnit) => void;
  fileSystem: FileSystemKind;
  setFileSystem: (f: FileSystemKind) => void;
  label: string;
  setLabel: (s: string) => void;
  newLabel: string;
  setNewLabel: (s: string) => void;
  resizeLimits: ResizeLimits | null;
  resizeNewBytes: number;
  setResizeNewBytes: (n: number) => void;
  onSubmit: () => void;
  onCancel: () => void;
}

function FormView(props: FormViewProps) {
  const {
    operation,
    sizeMode,
    setSizeMode,
    sizeAmount,
    setSizeAmount,
    sizeUnit,
    setSizeUnit,
    fileSystem,
    setFileSystem,
    label,
    setLabel,
    newLabel,
    setNewLabel,
    resizeLimits,
    resizeNewBytes,
    setResizeNewBytes,
    onSubmit,
    onCancel,
  } = props;

  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        onSubmit();
      }}
      className="space-y-4"
    >
      {operation.kind === "createPartition" && (
        <>
          <Field label="크기">
            <div className="flex gap-2">
              <select
                value={sizeMode}
                onChange={(e) =>
                  setSizeMode(e.target.value as "max" | "value")
                }
                className="rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-sm"
              >
                <option value="max">최대 가용</option>
                <option value="value">크기 지정</option>
              </select>
              {sizeMode === "value" && (
                <>
                  <input
                    type="number"
                    value={sizeAmount}
                    onChange={(e) => setSizeAmount(e.target.value)}
                    min={0}
                    step="any"
                    required
                    placeholder="예: 1.5"
                    className="flex-1 rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-sm"
                  />
                  <select
                    value={sizeUnit}
                    onChange={(e) => setSizeUnit(e.target.value as SizeUnit)}
                    className="rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-sm"
                  >
                    <option value="MB">MB</option>
                    <option value="GB">GB</option>
                    <option value="TB">TB</option>
                  </select>
                </>
              )}
            </div>
            {sizeMode === "value" && sizeAmount && (
              <p className="mt-1 text-[11px] text-neutral-500">
                ={" "}
                {Math.round(
                  Number(sizeAmount) * unitFactor(sizeUnit),
                ).toLocaleString()}{" "}
                B
              </p>
            )}
          </Field>

          <Field label="파일시스템">
            <select
              value={fileSystem}
              onChange={(e) => setFileSystem(e.target.value as FileSystemKind)}
              className="rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-sm"
            >
              <option value="FAT32">FAT32</option>
              <option value="exFAT">exFAT</option>
              <option value="NTFS">NTFS</option>
            </select>
          </Field>

          <Field label={`라벨 (선택, 최대 ${maxLabelLen(fileSystem)}자, ASCII)`}>
            <input
              type="text"
              value={label}
              onChange={(e) => setLabel(e.target.value)}
              maxLength={maxLabelLen(fileSystem)}
              placeholder="(없음)"
              className="w-full rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-sm"
            />
          </Field>
        </>
      )}

      {operation.kind === "setLabel" && (
        <Field
          label={`새 라벨 (최대 ${maxLabelLen(operation.partition.fileSystem)}자, ASCII)`}
        >
          <input
            type="text"
            value={newLabel}
            onChange={(e) => setNewLabel(e.target.value)}
            maxLength={maxLabelLen(operation.partition.fileSystem)}
            required
            autoFocus
            className="w-full rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-sm"
          />
          <p className="mt-1 text-xs text-neutral-500">
            현재 라벨: {operation.partition.label ?? "(없음)"}
          </p>
        </Field>
      )}

      {operation.kind === "deletePartition" && (
        <div className="rounded border border-red-900 bg-red-950/30 p-3 text-sm text-red-200 space-y-1">
          <p className="font-medium">⚠ 데이터 영구 손실</p>
          <p className="text-xs text-red-300">
            파티션 #{operation.partition.index}{" "}
            {operation.partition.driveLetter
              ? `(${operation.partition.driveLetter}:)`
              : ""}{" "}
            · {operation.partition.fileSystem} ·{" "}
            {formatBytes(operation.partition.sizeBytes)}
            {operation.partition.label
              ? ` · 라벨 "${operation.partition.label}"`
              : ""}
          </p>
          <p className="mt-2 text-xs text-red-300">
            이 파티션의 모든 데이터가 영구히 사라집니다. 복구할 수 없습니다.
          </p>
        </div>
      )}

      {operation.kind === "dismount" && (
        <div className="rounded border border-amber-900 bg-amber-950/30 p-3 text-sm text-amber-100 space-y-1">
          <p className="font-medium">드라이브 문자 제거</p>
          <p className="text-xs text-amber-200">
            파티션 #{operation.partition.index} (
            {operation.partition.driveLetter}:) 의 드라이브 문자만 제거합니다.
            데이터는 유지되며 다시 마운트할 수 있습니다.
          </p>
          <p className="mt-2 text-xs text-amber-300">
            보통 파티션 삭제 / 재포맷 전 단계로 사용합니다.
          </p>
        </div>
      )}

      {operation.kind === "resizePartition" && resizeLimits && (
        <ResizeForm
          limits={resizeLimits}
          newBytes={resizeNewBytes}
          setNewBytes={setResizeNewBytes}
        />
      )}

      <div className="flex justify-end gap-2 pt-2">
        <button
          type="button"
          onClick={onCancel}
          className="rounded px-4 py-1.5 text-sm text-neutral-400 hover:text-neutral-200"
        >
          취소
        </button>
        <button
          type="submit"
          className={`rounded px-4 py-1.5 text-sm font-medium ${
            operation.kind === "deletePartition"
              ? "bg-red-700 text-red-50 hover:bg-red-600"
              : "bg-amber-700 text-amber-50 hover:bg-amber-600"
          }`}
        >
          plan 미리보기
        </button>
      </div>
    </form>
  );
}

interface PreviewProps {
  disk: Disk;
  summary: string;
  typed: string;
  onTypedChange: (s: string) => void;
  onExecute: () => void;
  onCancel: () => void;
}

function PreviewView(props: PreviewProps) {
  const { disk, summary, typed, onTypedChange, onExecute, onCancel } = props;
  const typedMatches = typed === disk.model;

  return (
    <div className="space-y-4">
      <pre className="whitespace-pre-wrap rounded border border-amber-900 bg-amber-950/30 p-3 text-sm text-amber-100">
        {summary}
      </pre>
      <div className="rounded border border-red-900 bg-red-950/30 p-3 text-sm text-red-300">
        이 작업은 되돌릴 수 없을 수 있습니다. 확인하려면 디스크 모델명을 정확히
        입력하세요:
        <span className="ml-1 font-mono text-red-200">{disk.model}</span>
      </div>
      <input
        type="text"
        value={typed}
        onChange={(e) => onTypedChange(e.target.value)}
        autoFocus
        spellCheck={false}
        className="w-full rounded border border-neutral-700 bg-neutral-800 px-2 py-1.5 font-mono text-sm"
      />
      <div className="flex justify-end gap-2">
        <button
          type="button"
          onClick={onCancel}
          className="rounded px-4 py-1.5 text-sm text-neutral-400 hover:text-neutral-200"
        >
          취소
        </button>
        <button
          type="button"
          onClick={onExecute}
          disabled={!typedMatches}
          className="rounded bg-red-700 px-4 py-1.5 text-sm font-medium text-red-50 hover:bg-red-600 disabled:cursor-not-allowed disabled:bg-neutral-700 disabled:text-neutral-500"
        >
          실행
        </button>
      </div>
    </div>
  );
}

function Field({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <label className="block">
      <span className="mb-1 block text-xs uppercase tracking-wide text-neutral-400">
        {label}
      </span>
      {children}
    </label>
  );
}

interface ResizeFormProps {
  limits: ResizeLimits;
  newBytes: number;
  setNewBytes: (n: number) => void;
}

function ResizeForm({ limits, newBytes, setNewBytes }: ResizeFormProps) {
  // 입력 단계 단위는 사용자 경험을 위해 1MB.
  const STEP = 1_000_000;

  // 사용자가 MB / GB / TB 중 편한 단위로 정확한 숫자를 입력할 수도 있음.
  // 디스크 크기에 따라 합리적인 기본 단위 선택.
  const [unit, setUnit] = useState<SizeUnit>(() =>
    limits.maxBytes >= 1_000_000_000_000
      ? "TB"
      : limits.maxBytes >= 1_000_000_000
        ? "GB"
        : "MB",
  );
  const factor = unitFactor(unit);
  const decimals = unit === "TB" ? 3 : unit === "GB" ? 2 : 0;
  const valueInUnit = (newBytes / factor).toFixed(decimals);

  const onSliderChange = (s: string) => {
    const n = Number(s);
    if (Number.isFinite(n)) setNewBytes(n);
  };

  const onUnitInputChange = (s: string) => {
    const n = Number(s);
    if (Number.isFinite(n) && n >= 0) {
      setNewBytes(Math.round(n * factor));
    }
  };

  const direction =
    newBytes > limits.currentBytes
      ? `+${formatBytes(newBytes - limits.currentBytes)} 확장`
      : newBytes < limits.currentBytes
        ? `-${formatBytes(limits.currentBytes - newBytes)} 축소`
        : "변경 없음";

  return (
    <div className="space-y-3">
      <div className="text-xs text-neutral-400 space-y-0.5">
        <p>
          <span className="text-neutral-500">현재:</span>{" "}
          {formatBytes(limits.currentBytes)} ({limits.currentBytes.toLocaleString()} B)
        </p>
        <p>
          <span className="text-neutral-500">최소 (Windows 한계):</span>{" "}
          {formatBytes(limits.minBytes)}
        </p>
        <p>
          <span className="text-neutral-500">최대 (인접 미할당 포함):</span>{" "}
          {formatBytes(limits.maxBytes)}
        </p>
      </div>

      <Field label="새 크기">
        <input
          type="range"
          min={limits.minBytes}
          max={limits.maxBytes}
          step={STEP}
          value={newBytes}
          onChange={(e) => onSliderChange(e.target.value)}
          className="w-full"
        />
      </Field>

      <div className="flex items-end gap-2">
        <input
          type="number"
          min={0}
          step="any"
          value={valueInUnit}
          onChange={(e) => onUnitInputChange(e.target.value)}
          className="flex-1 rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-sm"
        />
        <select
          value={unit}
          onChange={(e) => setUnit(e.target.value as SizeUnit)}
          className="rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-sm"
        >
          <option value="MB">MB</option>
          <option value="GB">GB</option>
          <option value="TB">TB</option>
        </select>
      </div>

      <div className="rounded border border-amber-900 bg-amber-950/30 p-2 text-xs text-amber-200">
        {direction} · {formatBytes(newBytes)}
      </div>

      {newBytes < limits.currentBytes && (
        <p className="text-[11px] text-neutral-500">
          축소 시 Windows 가 immovable 파일 (MFT, 페이지파일 등) 위치에 의해 제한됩니다.
        </p>
      )}
    </div>
  );
}
