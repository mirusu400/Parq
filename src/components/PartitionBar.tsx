import { useRef, useState } from "react";
import type { Disk, Partition } from "../types";
import type { Operation } from "./OperationModal";
import { formatBytes } from "../lib/format";

interface Props {
  disk: Disk;
  onOperation: (op: Operation) => void;
  /** V2 destructive 알파 게이트 상태. 꺼져 있으면 이동 버튼 미노출. */
  v2Enabled: boolean;
  onMove: (disk: Disk, partition: Partition) => void;
}

interface Segment {
  kind: "partition" | "unallocated";
  partition: Partition | null;
  offsetBytes: number;
  sizeBytes: number;
}

function buildSegments(disk: Disk): Segment[] {
  const sorted = [...disk.partitions].sort(
    (a, b) => a.offsetBytes - b.offsetBytes,
  );
  const segments: Segment[] = [];
  let cursor = 0;

  for (const p of sorted) {
    if (p.offsetBytes > cursor) {
      segments.push({
        kind: "unallocated",
        partition: null,
        offsetBytes: cursor,
        sizeBytes: p.offsetBytes - cursor,
      });
    }
    segments.push({
      kind: "partition",
      partition: p,
      offsetBytes: p.offsetBytes,
      sizeBytes: p.sizeBytes,
    });
    cursor = p.offsetBytes + p.sizeBytes;
  }

  if (cursor < disk.sizeBytes) {
    segments.push({
      kind: "unallocated",
      partition: null,
      offsetBytes: cursor,
      sizeBytes: disk.sizeBytes - cursor,
    });
  }

  return segments;
}

function fillFor(seg: Segment): string {
  if (seg.kind === "unallocated") return "bg-neutral-700";
  const p = seg.partition!;
  if (p.isSystem) return "bg-amber-700";
  switch (p.fileSystem) {
    case "NTFS":
      return "bg-sky-600";
    case "FAT32":
      return "bg-emerald-600";
    case "exFAT":
      return "bg-teal-600";
    case "ReFS":
      return "bg-indigo-600";
    case "EFI":
      return "bg-amber-700";
    default:
      return "bg-neutral-600";
  }
}

const RELABELABLE_FS = ["FAT32", "exFAT", "NTFS"];

function canRelabel(disk: Disk, p: Partition): boolean {
  return (
    disk.isWritableV1 &&
    p.bitlockerStatus === "NotEncrypted" &&
    !p.isBoot &&
    !p.isSystem &&
    p.driveLetter !== null &&
    RELABELABLE_FS.includes(p.fileSystem)
  );
}

function canDelete(disk: Disk, p: Partition): boolean {
  // V1 destructive 가드는 마운트 해제 (drive letter 없음) 를 요구한다.
  return (
    disk.isWritableV1 &&
    p.bitlockerStatus === "NotEncrypted" &&
    !p.isBoot &&
    !p.isSystem &&
    !p.isInUse
  );
}

function canDismount(disk: Disk, p: Partition): boolean {
  // 드라이브 문자가 있어야 제거 의미가 있음. 메타 작업이라 마운트 상태에서도 허용.
  return (
    disk.isWritableV1 &&
    p.bitlockerStatus === "NotEncrypted" &&
    !p.isBoot &&
    !p.isSystem &&
    p.driveLetter !== null
  );
}

function canResize(disk: Disk, p: Partition): boolean {
  const systemBootVolume =
    disk.isSystem && p.isBoot && !p.isSystem && p.driveLetter !== null;
  const offlineDataVolume =
    disk.isWritableV1 && !p.isBoot && !p.isSystem && !p.isInUse;
  return (
    p.bitlockerStatus === "NotEncrypted" &&
    p.fileSystem === "NTFS" &&
    (systemBootVolume || offlineDataVolume)
  );
}

function canMove(disk: Disk, p: Partition, v2Enabled: boolean): boolean {
  // V2 이동: 알파 게이트 ON + 쓰기 가능 + MBR/GPT + 부팅/시스템 아님 +
  // 마운트 해제 상태 (destructive 가드).
  return (
    v2Enabled &&
    disk.isWritableV1 &&
    p.bitlockerStatus === "NotEncrypted" &&
    (disk.partitionStyle === "MBR" || disk.partitionStyle === "GPT") &&
    !p.isBoot &&
    !p.isSystem &&
    !p.isInUse
  );
}

// 드래그-리사이즈 안전 floor. 백엔드의 정확한 min 은 plan_resize_partition 이 검증함 —
// 여기는 사용자가 음수/0 영역으로 드래그 못 하게만 막는 가드.
const DRAG_MIN_BYTES = 10 * 1_000_000;

interface DragState {
  segIndex: number;
  startSizeBytes: number;
  neighborStartBytes: number;
  hasUnallocatedNeighbor: boolean;
  currentNewSizeBytes: number;
}

export default function PartitionBar({
  disk,
  onOperation,
  v2Enabled,
  onMove,
}: Props) {
  const segments = buildSegments(disk);
  const totalForLayout = Math.max(disk.sizeBytes, 1);
  const barRef = useRef<HTMLDivElement>(null);
  const [drag, setDrag] = useState<DragState | null>(null);

  const startDrag = (segIndex: number, e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    const barEl = barRef.current;
    if (!barEl) return;
    const seg = segments[segIndex];
    if (seg.kind !== "partition" || !seg.partition) return;
    const partition = seg.partition;
    const next = segments[segIndex + 1];
    const hasUnalloc = !!next && next.kind === "unallocated";
    const startMouseX = e.clientX;
    const startSizeBytes = seg.sizeBytes;
    const neighborBytes = hasUnalloc && next ? next.sizeBytes : 0;
    const totalPx = barEl.getBoundingClientRect().width;
    if (totalPx <= 0) return;

    setDrag({
      segIndex,
      startSizeBytes,
      neighborStartBytes: neighborBytes,
      hasUnallocatedNeighbor: hasUnalloc,
      currentNewSizeBytes: startSizeBytes,
    });

    let latestNewSize = startSizeBytes;

    const onMove = (ev: MouseEvent) => {
      const deltaPx = ev.clientX - startMouseX;
      const bytesPerPx = disk.sizeBytes / totalPx;
      // 인접 unallocated 가 있으면 그만큼 extend 가능, 없으면 shrink 만.
      const upperBound = hasUnalloc
        ? startSizeBytes + neighborBytes
        : startSizeBytes;
      const raw = startSizeBytes + Math.round(deltaPx * bytesPerPx);
      const clamped = Math.min(Math.max(raw, DRAG_MIN_BYTES), upperBound);
      latestNewSize = clamped;
      setDrag((d) =>
        d ? { ...d, currentNewSizeBytes: clamped } : null,
      );
    };

    const onUp = () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      // 의미 있는 변화가 없으면 모달 안 띄움 (실수 클릭 방지).
      if (latestNewSize !== startSizeBytes) {
        onOperation({
          kind: "resizePartition",
          disk,
          partition,
          initialSizeBytes: latestNewSize,
        });
      }
      setDrag(null);
    };

    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  };

  // 드래그 중에는 해당 파티션 + 인접 unallocated 의 크기를 오버라이드해서 라이브 프리뷰.
  const displaySegments: Segment[] = drag
    ? segments.map((s, i) => {
        if (i === drag.segIndex) {
          return { ...s, sizeBytes: drag.currentNewSizeBytes };
        }
        if (
          drag.hasUnallocatedNeighbor &&
          i === drag.segIndex + 1
        ) {
          const delta = drag.currentNewSizeBytes - drag.startSizeBytes;
          return { ...s, sizeBytes: drag.neighborStartBytes - delta };
        }
        return s;
      })
    : segments;

  return (
    <div className="space-y-2">
      <div
        ref={barRef}
        className={`relative flex h-10 w-full overflow-hidden rounded border border-neutral-800 ${drag ? "select-none" : ""}`}
        role="img"
        aria-label={`${disk.model} 파티션 레이아웃`}
      >
        {displaySegments.map((seg, i) => {
          const widthPct = (seg.sizeBytes / totalForLayout) * 100;
          const label =
            seg.kind === "partition"
              ? (seg.partition?.driveLetter ?? seg.partition?.label ?? "?")
              : "";
          const isResizable =
            seg.kind === "partition" &&
            seg.partition &&
            canResize(disk, seg.partition);
          return (
            <div
              key={i}
              className={`relative flex items-center justify-center ${fillFor(seg)} text-xs text-neutral-50`}
              style={{ width: `${widthPct}%` }}
              title={
                seg.kind === "partition"
                  ? `${seg.partition?.label ?? ""} (${seg.partition?.fileSystem}, ${formatBytes(seg.sizeBytes)})`
                  : `미할당 (${formatBytes(seg.sizeBytes)})`
              }
            >
              {widthPct > 6 && label}
              {isResizable && (
                <div
                  onMouseDown={(e) => startDrag(i, e)}
                  className="absolute right-0 top-0 z-10 h-full w-1.5 cursor-ew-resize bg-amber-400/0 hover:bg-amber-300/70"
                  title="드래그하여 크기 조절"
                />
              )}
            </div>
          );
        })}
      </div>
      {drag && (
        <div className="text-xs text-amber-300">
          새 크기: {formatBytes(drag.currentNewSizeBytes)} (변경량{" "}
          {drag.currentNewSizeBytes >= drag.startSizeBytes ? "+" : "−"}
          {formatBytes(
            Math.abs(drag.currentNewSizeBytes - drag.startSizeBytes),
          )}
          ) — 놓으면 확인 모달이 열립니다
        </div>
      )}
      <ul className="space-y-1 text-xs text-neutral-400">
        {segments.map((seg, i) => (
          <li key={i} className="flex items-center justify-between gap-2">
            <span className="flex items-center gap-1.5">
              <span
                className={`inline-block h-2.5 w-2.5 rounded-sm ${fillFor(seg)}`}
              />
              <span>
                {seg.kind === "partition"
                  ? `${seg.partition?.driveLetter ? seg.partition.driveLetter + ": " : ""}${seg.partition?.label ?? "(라벨 없음)"} · ${seg.partition?.fileSystem} · ${formatBytes(seg.sizeBytes)}${seg.partition?.bitlockerStatus !== "NotEncrypted" ? ` · BitLocker ${seg.partition?.bitlockerStatus}` : ""}`
                  : `미할당 · ${formatBytes(seg.sizeBytes)}`}
              </span>
            </span>
            {seg.kind === "partition" && seg.partition && (
              <span className="flex gap-1">
                {canRelabel(disk, seg.partition) && (
                  <button
                    type="button"
                    onClick={() =>
                      onOperation({
                        kind: "setLabel",
                        disk,
                        partition: seg.partition!,
                      })
                    }
                    className="rounded border border-neutral-700 px-2 py-0.5 text-[11px] text-neutral-300 hover:border-amber-700 hover:text-amber-200"
                  >
                    라벨 변경
                  </button>
                )}
                {canResize(disk, seg.partition) && (
                  <button
                    type="button"
                    onClick={() =>
                      onOperation({
                        kind: "resizePartition",
                        disk,
                        partition: seg.partition!,
                      })
                    }
                    className="rounded border border-neutral-700 px-2 py-0.5 text-[11px] text-neutral-300 hover:border-amber-700 hover:text-amber-200"
                  >
                    리사이즈
                  </button>
                )}
                {canDismount(disk, seg.partition) && (
                  <button
                    type="button"
                    onClick={() =>
                      onOperation({
                        kind: "dismount",
                        disk,
                        partition: seg.partition!,
                      })
                    }
                    className="rounded border border-neutral-700 px-2 py-0.5 text-[11px] text-neutral-300 hover:border-amber-700 hover:text-amber-200"
                  >
                    문자 제거
                  </button>
                )}
                {canDelete(disk, seg.partition) && (
                  <button
                    type="button"
                    onClick={() =>
                      onOperation({
                        kind: "deletePartition",
                        disk,
                        partition: seg.partition!,
                      })
                    }
                    className="rounded border border-neutral-700 px-2 py-0.5 text-[11px] text-neutral-300 hover:border-red-700 hover:text-red-200"
                  >
                    삭제
                  </button>
                )}
                {canMove(disk, seg.partition, v2Enabled) && (
                  <button
                    type="button"
                    onClick={() => onMove(disk, seg.partition!)}
                    className="rounded border border-amber-800 px-2 py-0.5 text-[11px] text-amber-300 hover:border-amber-600 hover:text-amber-100"
                    title="V2: 파티션을 미할당 영역으로 이동 (MBR)"
                  >
                    이동 (V2)
                  </button>
                )}
              </span>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}
