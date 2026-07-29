import type { Disk, Partition } from "../types";
import type { Operation } from "./OperationModal";
import { formatBytes } from "../lib/format";
import PartitionBar from "./PartitionBar";

interface Props {
  disks: Disk[];
  onOperation: (op: Operation) => void;
  v2Enabled: boolean;
  onMove: (disk: Disk, partition: Partition) => void;
}

function busTypeLabel(disk: Disk): string {
  return disk.isRemovable ? `${disk.busType} · 제거 가능` : disk.busType;
}

function freeBytesOf(disk: Disk): number {
  const used = disk.partitions.reduce((s, p) => s + p.sizeBytes, 0);
  return Math.max(0, disk.sizeBytes - used);
}

export default function DiskList({
  disks,
  onOperation,
  v2Enabled,
  onMove,
}: Props) {
  if (disks.length === 0) {
    return (
      <p className="text-sm text-neutral-500">표시할 디스크가 없습니다.</p>
    );
  }

  return (
    <div className="space-y-4">
      {disks.map((disk) => {
        const free = freeBytesOf(disk);
        const canCreate = disk.isWritableV1 && free > 0;
        return (
          <article
            key={disk.id}
            className="rounded-lg border border-neutral-800 bg-neutral-900 p-5"
          >
            <header className="mb-4 flex items-start justify-between gap-4">
              <div>
                <h2 className="text-lg font-medium">
                  디스크 {disk.number} · {disk.model}
                </h2>
                <p className="mt-0.5 text-xs text-neutral-500">
                  {busTypeLabel(disk)} · {disk.partitionStyle} ·{" "}
                  {formatBytes(disk.sizeBytes)}
                  {disk.serial ? ` · S/N ${disk.serial}` : ""}
                  {free > 0 && ` · 미할당 ${formatBytes(free)}`}
                </p>
              </div>
              <div className="flex flex-col items-end gap-1">
                {disk.isSystem && (
                  <span
                    className="rounded border border-amber-700 px-2 py-0.5 text-xs text-amber-400"
                    title="시스템 디스크 — V1 에서는 read-only"
                  >
                    시스템 디스크 (보호됨)
                  </span>
                )}
                {!disk.isWritableV1 && !disk.isSystem && (
                  <span
                    className="rounded border border-neutral-700 px-2 py-0.5 text-xs text-neutral-500"
                    title="V1 외장 화이트리스트 미통과 — PARQ_DEV_ALLOW_INTERNAL_DISKS 환경변수로 우회 가능"
                  >
                    쓰기 차단됨
                  </span>
                )}
                {canCreate && (
                  <button
                    type="button"
                    onClick={() => onOperation({ kind: "createPartition", disk })}
                    className="rounded border border-amber-700 bg-amber-900/40 px-3 py-1 text-xs font-medium text-amber-200 hover:bg-amber-800/60"
                  >
                    파티션 생성
                  </button>
                )}
              </div>
            </header>
            <PartitionBar
              disk={disk}
              onOperation={onOperation}
              v2Enabled={v2Enabled}
              onMove={onMove}
            />
          </article>
        );
      })}
    </div>
  );
}
