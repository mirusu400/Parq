// V2 파티션 이동 모달 (destructive, 알파 게이트 뒤에서만 진입).
//
// 흐름은 다른 파괴적 작업과 동일: form → plan(미리보기) → 디스크 모델명 타이핑 확인 → execute.
// 이동은 데이터를 옮기는 파괴적 작업이지만 checkpoint + SHA256 라운드트립으로 검증되고 되돌릴
// 수 있으므로 빨강이 아닌 주황 경고를 쓴다(브랜딩 가이드).

import { useState } from "react";
import type { Disk, MovePartitionPlan, Partition } from "../types";
import {
  executeMovePartition,
  planMovePartition,
} from "../lib/operations";
import { formatBytes } from "../lib/format";

const MIB = 1024 * 1024;

interface Props {
  disk: Disk;
  partition: Partition;
  onClose: () => void;
  onSuccess: () => void;
}

type Phase =
  | { stage: "form" }
  | { stage: "loadingPlan" }
  | { stage: "preview"; plan: MovePartitionPlan; typed: string }
  | { stage: "loadingExecute"; plan: MovePartitionPlan }
  | { stage: "done"; newStartLba: number }
  | { stage: "error"; message: string };

function errMessage(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
}

export default function MovePartitionModal({
  disk,
  partition,
  onClose,
  onSuccess,
}: Props) {
  // 새 시작 오프셋 (MiB). 기본값: 현재 오프셋 (사용자가 free 영역 오프셋으로 변경).
  const [newStartMib, setNewStartMib] = useState<number>(
    Math.round(partition.offsetBytes / MIB),
  );
  const [phase, setPhase] = useState<Phase>({ stage: "form" });

  const newStartBytes = Math.round(newStartMib) * MIB;

  const loadPlan = () => {
    setPhase({ stage: "loadingPlan" });
    planMovePartition(disk.id, partition.id, newStartBytes)
      .then((plan) => setPhase({ stage: "preview", plan, typed: "" }))
      .catch((e) => setPhase({ stage: "error", message: errMessage(e) }));
  };

  const execute = (plan: MovePartitionPlan) => {
    setPhase({ stage: "loadingExecute", plan });
    executeMovePartition(disk.id, partition.id, newStartBytes)
      .then((res) => {
        setPhase({ stage: "done", newStartLba: res.newStartLba });
      })
      .catch((e) => setPhase({ stage: "error", message: errMessage(e) }));
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4">
      <div className="w-full max-w-lg rounded-lg border border-amber-800 bg-neutral-900 p-6">
        <header className="mb-4">
          <h2 className="text-lg font-semibold text-amber-200">
            파티션 이동 (V2 · 알파)
          </h2>
          <p className="mt-1 text-xs text-neutral-400">
            디스크 {disk.number} · {disk.model} ·{" "}
            {partition.driveLetter ? partition.driveLetter + ": " : ""}
            {partition.label ?? "(라벨 없음)"} · {partition.fileSystem} ·{" "}
            {formatBytes(partition.sizeBytes)}
          </p>
        </header>

        <div className="mb-4 rounded border border-amber-800 bg-amber-950/40 p-3 text-xs text-amber-200">
          이동은 파티션 데이터를 새 위치로 복사하고 파티션 테이블을 갱신합니다. 진행 중 전원이
          꺼져도 checkpoint 에서 재개하며, 이동 후 SHA256 라운드트립으로 무결성을 검증합니다.
          <span className="text-amber-400"> 현재 오프라인 MBR/GPT 데이터 파티션을 지원합니다.</span>
        </div>

        {phase.stage === "form" && (
          <div className="space-y-4">
            <label className="block text-sm">
              <span className="text-neutral-300">새 시작 오프셋 (MiB)</span>
              <input
                type="number"
                min={1}
                value={newStartMib}
                onChange={(e) => setNewStartMib(Number(e.target.value))}
                className="mt-1 w-full rounded border border-neutral-700 bg-neutral-800 px-3 py-2 text-sm"
              />
              <span className="mt-1 block text-xs text-neutral-500">
                현재: {formatBytes(partition.offsetBytes)} (
                {Math.round(partition.offsetBytes / MIB)} MiB) · 새 위치:{" "}
                {formatBytes(newStartBytes)} · 미할당 영역으로만 이동 가능
              </span>
            </label>
            <div className="flex justify-end gap-2">
              <button
                type="button"
                onClick={onClose}
                className="rounded border border-neutral-700 px-3 py-1.5 text-sm text-neutral-300 hover:bg-neutral-800"
              >
                취소
              </button>
              <button
                type="button"
                onClick={loadPlan}
                disabled={newStartBytes === partition.offsetBytes}
                className="rounded border border-amber-700 bg-amber-900/40 px-3 py-1.5 text-sm font-medium text-amber-200 hover:bg-amber-800/60 disabled:opacity-40"
              >
                미리보기
              </button>
            </div>
          </div>
        )}

        {phase.stage === "loadingPlan" && (
          <p className="text-sm text-neutral-400">이동 계획 계산 중...</p>
        )}

        {phase.stage === "preview" && (
          <div className="space-y-4">
            <div className="rounded border border-neutral-700 bg-neutral-800/60 p-3 text-sm">
              <p className="text-neutral-200">{phase.plan.summary}</p>
              <p className="mt-2 text-xs text-neutral-400">
                방향: {phase.plan.direction} · 길이:{" "}
                {phase.plan.lengthSectors.toLocaleString()} sectors · LBA{" "}
                {phase.plan.srcStartLba.toLocaleString()} →{" "}
                {phase.plan.newStartLba.toLocaleString()}
              </p>
            </div>
            <label className="block text-sm">
              <span className="text-neutral-300">
                확인을 위해 디스크 모델명{" "}
                <code className="text-amber-300">{disk.model}</code> 을 입력하세요
              </span>
              <input
                type="text"
                value={phase.typed}
                onChange={(e) =>
                  setPhase({ ...phase, typed: e.target.value })
                }
                className="mt-1 w-full rounded border border-neutral-700 bg-neutral-800 px-3 py-2 text-sm"
                autoFocus
              />
            </label>
            <div className="flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setPhase({ stage: "form" })}
                className="rounded border border-neutral-700 px-3 py-1.5 text-sm text-neutral-300 hover:bg-neutral-800"
              >
                뒤로
              </button>
              <button
                type="button"
                disabled={phase.typed !== disk.model}
                onClick={() => execute(phase.plan)}
                className="rounded border border-amber-600 bg-amber-800/60 px-3 py-1.5 text-sm font-semibold text-amber-100 hover:bg-amber-700/70 disabled:opacity-40"
              >
                이동 실행
              </button>
            </div>
          </div>
        )}

        {phase.stage === "loadingExecute" && (
          <p className="text-sm text-amber-300">
            이동 중... (전원을 끄지 마세요. 중단돼도 checkpoint 에서 재개됩니다)
          </p>
        )}

        {phase.stage === "done" && (
          <div className="space-y-4">
            <p className="text-sm text-emerald-300">
              이동 완료. 파티션이 LBA {phase.newStartLba.toLocaleString()} 로
              이동됐고 SHA256 라운드트립이 검증됐습니다.
            </p>
            <div className="flex justify-end">
              <button
                type="button"
                onClick={() => {
                  onSuccess();
                  onClose();
                }}
                className="rounded border border-emerald-700 bg-emerald-900/40 px-3 py-1.5 text-sm text-emerald-200 hover:bg-emerald-800/60"
              >
                닫기
              </button>
            </div>
          </div>
        )}

        {phase.stage === "error" && (
          <div className="space-y-4">
            <div className="rounded border border-red-900 bg-red-950/40 p-3 text-sm text-red-300">
              <p className="font-medium">이동 실패</p>
              <p className="mt-1 text-xs text-red-400">{phase.message}</p>
            </div>
            <div className="flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setPhase({ stage: "form" })}
                className="rounded border border-neutral-700 px-3 py-1.5 text-sm text-neutral-300 hover:bg-neutral-800"
              >
                다시
              </button>
              <button
                type="button"
                onClick={onClose}
                className="rounded border border-neutral-700 px-3 py-1.5 text-sm text-neutral-300 hover:bg-neutral-800"
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
