// 백엔드 (Tauri command) 와 공유하는 도메인 타입 선언.
//
// 백엔드의 Rust struct (serde) 와 형태가 일치해야 한다.
// 수정 시 양쪽을 함께 갱신하고, 통합 테스트로 round-trip 을 검증한다.

export type BusType =
  | "USB"
  | "SD"
  | "MMC"
  | "IEEE1394"
  | "SATA"
  | "NVMe"
  | "SCSI"
  | "Virtual"
  | "Unknown";

export type PartitionStyle = "MBR" | "GPT" | "RAW";

export type FileSystemKind =
  | "NTFS"
  | "FAT32"
  | "exFAT"
  | "ReFS"
  | "EFI"
  | "Unknown";

export type BitLockerStatus =
  | "NotEncrypted"
  | "Encrypted"
  | "Unknown";

export interface Disk {
  /** 안정적인 식별자 (시리얼 또는 디스크 ID). 디스크 번호는 매번 바뀌므로 사용 금지. */
  id: string;
  number: number;
  model: string;
  serial: string | null;
  sizeBytes: number;
  busType: BusType;
  partitionStyle: PartitionStyle;
  isRemovable: boolean;
  isSystem: boolean;
  isReadOnly: boolean;
  partitions: Partition[];
  /** 백엔드 safety V1 가드 (PARQ_DEV_ALLOW_INTERNAL_DISKS 환경변수 반영) 통과 여부. */
  isWritableV1: boolean;
}

export interface Partition {
  id: string;
  index: number;
  /** 디스크 시작 LBA */
  offsetBytes: number;
  sizeBytes: number;
  driveLetter: string | null;
  label: string | null;
  fileSystem: FileSystemKind;
  isBoot: boolean;
  isSystem: boolean;
  isHidden: boolean;
  bitlockerStatus: BitLockerStatus;
  /** 마운트되어 사용 중이면 true — V1 에서 쓰기 작업 차단의 1차 신호. */
  isInUse: boolean;
}

// ===== 파괴적 작업 plan / 파라미터 타입 — backend partition 모듈과 일치해야 함 =====

// 백엔드 partition::SizeRequest 와 일치 (serde rename_all = "camelCase").
export type SizeRequest =
  | { kind: "bytes"; value: number }
  | { kind: "useMaximum" };

export interface CreatePartitionPlan {
  disk: Disk;
  sizeRequest: SizeRequest;
  fileSystem: FileSystemKind;
  label: string | null;
  initializeAsGpt: boolean;
  summary: string;
}

export interface SetLabelPlan {
  disk: Disk;
  partition: Partition;
  newLabel: string;
  summary: string;
}

export interface DeletePartitionPlan {
  disk: Disk;
  partition: Partition;
  summary: string;
}

export interface DismountPlan {
  disk: Disk;
  partition: Partition;
  driveLetter: string;
  summary: string;
}

export interface ResizeLimits {
  currentBytes: number;
  minBytes: number;
  maxBytes: number;
}

export interface ResizePartitionPlan {
  disk: Disk;
  partition: Partition;
  newSizeBytes: number;
  currentSizeBytes: number;
  minSupportedBytes: number;
  maxSupportedBytes: number;
  summary: string;
}

// ===== V2 파티션 이동 (destructive, 알파 게이트 뒤) — backend commands::v2 DTO 와 일치 =====

export interface MovePartitionPlan {
  diskNumber: number;
  srcStartLba: number;
  newStartLba: number;
  lengthSectors: number;
  /** "Forward" | "Backward" */
  direction: string;
  summary: string;
}

export interface MovePartitionResult {
  partitionId: string;
  oldStartLba: number;
  newStartLba: number;
  lengthSectors: number;
  sha256: string;
  resumed: boolean;
}

// ===== 트랜잭션 로그 (read-only) =====
//
// 다른 IPC 타입과 달리 snake_case 필드를 사용한다 — 백엔드 transaction::TransactionLog
// 가 디스크에 직접 fsync 하는 JSON 파일 포맷이고, 그 포맷이 디버깅 / 수동 분석의 1차
// 인터페이스이기 때문에 일관된 한 가지 표기를 유지한다 (snake_case).
// 기존에 저장된 로그 파일과의 호환성을 위해서도 변경 불가.

export type StepStatus = "pending" | "running" | "done" | "failed";

export interface TransactionStep {
  name: string;
  started_at_unix_nanos: number;
  ended_at_unix_nanos: number | null;
  status: StepStatus;
  detail: string | null;
}

export interface TransactionLog {
  id: string;
  started_at_unix_nanos: number;
  operation: string;
  disk_id: string;
  disk_summary: string;
  plan_summary: string;
  steps: TransactionStep[];
  ended_at_unix_nanos: number | null;
  /** "committed" | "failed: ..." | legacy "rolled_back: ..." | "dropped_without_finalize" | null */
  result: string | null;
}
