// 파괴적 (write) 작업의 IPC 래퍼.
// 백엔드 commands::write::* 와 1:1 매칭. plan/execute 분리는 안전 모델의 핵심 —
// frontend 에서 plan 받아 사용자 확인을 받은 다음에만 execute_*_dangerous 호출한다.

import { invoke } from "@tauri-apps/api/core";
import type {
  CreatePartitionPlan,
  DeletePartitionPlan,
  DismountPlan,
  FileSystemKind,
  MovePartitionPlan,
  MovePartitionResult,
  ResizeLimits,
  ResizePartitionPlan,
  SetLabelPlan,
  SizeRequest,
} from "../types";

export async function planCreatePartition(
  diskId: string,
  sizeRequest: SizeRequest,
  fileSystem: FileSystemKind,
  label: string | null,
): Promise<CreatePartitionPlan> {
  return invoke<CreatePartitionPlan>("plan_create_partition", {
    diskId,
    sizeRequest,
    fileSystem,
    label,
  });
}

export async function executeCreatePartition(
  plan: CreatePartitionPlan,
): Promise<void> {
  await invoke("execute_create_partition_dangerous", { plan });
}

export async function planSetLabel(
  diskId: string,
  partitionId: string,
  newLabel: string,
): Promise<SetLabelPlan> {
  return invoke<SetLabelPlan>("plan_set_label", {
    diskId,
    partitionId,
    newLabel,
  });
}

export async function executeSetLabel(plan: SetLabelPlan): Promise<void> {
  await invoke("execute_set_label_dangerous", { plan });
}

export async function planDeletePartition(
  diskId: string,
  partitionId: string,
): Promise<DeletePartitionPlan> {
  return invoke<DeletePartitionPlan>("plan_delete_partition", {
    diskId,
    partitionId,
  });
}

export async function executeDeletePartition(
  plan: DeletePartitionPlan,
): Promise<void> {
  await invoke("execute_delete_partition_dangerous", { plan });
}

export async function planDismount(
  diskId: string,
  partitionId: string,
): Promise<DismountPlan> {
  return invoke<DismountPlan>("plan_dismount", { diskId, partitionId });
}

export async function executeDismount(plan: DismountPlan): Promise<void> {
  await invoke("execute_dismount_dangerous", { plan });
}

export async function getResizeLimits(
  diskId: string,
  partitionId: string,
): Promise<ResizeLimits> {
  return invoke<ResizeLimits>("get_resize_limits", { diskId, partitionId });
}

export async function planResizePartition(
  diskId: string,
  partitionId: string,
  newSizeBytes: number,
): Promise<ResizePartitionPlan> {
  return invoke<ResizePartitionPlan>("plan_resize_partition", {
    diskId,
    partitionId,
    newSizeBytes,
  });
}

export async function executeResizePartition(
  plan: ResizePartitionPlan,
): Promise<void> {
  await invoke("execute_resize_partition_dangerous", { plan });
}

// ===== V2 파티션 이동 (알파 게이트 뒤) =====

/** V2 destructive 알파 게이트가 켜져 있는지. UI 가 이동 컨트롤 노출 여부를 결정. */
export async function v2DestructiveEnabled(): Promise<boolean> {
  return invoke<boolean>("v2_destructive_enabled");
}

export async function planMovePartition(
  diskId: string,
  partitionId: string,
  newStartBytes: number,
): Promise<MovePartitionPlan> {
  return invoke<MovePartitionPlan>("plan_move_partition", {
    diskId,
    partitionId,
    newStartBytes,
  });
}

export async function executeMovePartition(
  diskId: string,
  partitionId: string,
  newStartBytes: number,
): Promise<MovePartitionResult> {
  return invoke<MovePartitionResult>("execute_move_partition_dangerous", {
    diskId,
    partitionId,
    newStartBytes,
  });
}
