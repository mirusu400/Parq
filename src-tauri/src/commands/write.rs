// 파괴적 (write) Tauri commands.
//
// 컨벤션:
// - 함수명에 `_dangerous` 접미사
// - 모든 command 는 입력 검증 → safety guard → transaction 흐름
// - frontend 에 보내는 에러는 사람이 읽을 수 있는 한국어 문자열
//
// Plan / Execute 분리:
// - `plan_*` : read-only. plan 객체를 frontend 로 반환해 미리보기
// - `execute_*_dangerous` : plan 을 받아 transaction 안에서 실행

use tracing::{error, instrument};

use crate::disk::{self, Disk, FileSystemKind};
use crate::partition::{
    self, CreatePartitionPlan, DeletePartitionPlan, DismountPlan, ResizeLimits,
    ResizePartitionPlan, SetLabelPlan, SizeRequest,
};
use crate::ParqError;

fn fetch_disk(disk_id: &str) -> Result<Disk, ParqError> {
    let disks = disk::enumerate()?;
    disks
        .into_iter()
        .find(|d| d.id == disk_id)
        .ok_or_else(|| ParqError::DiskNotFound(disk_id.to_string()))
}

/// 파티션 생성 plan 을 계산해 frontend 로 반환한다. **read-only**.
#[tauri::command]
#[instrument(skip(label))]
pub async fn plan_create_partition(
    disk_id: String,
    size_request: SizeRequest,
    file_system: FileSystemKind,
    label: Option<String>,
) -> Result<CreatePartitionPlan, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<CreatePartitionPlan, ParqError> {
        let disk = fetch_disk(&disk_id)?;
        partition::plan_create_partition(&disk, size_request, file_system, label)
    })
    .await
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "plan_create_partition 워커 패닉");
        format!("plan_create_partition 작업이 비정상 종료되었습니다: {e}")
    })?
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "plan_create_partition 실패");
        e.to_string()
    })
}

/// `plan_create_partition` 으로 받은 plan 을 그대로 받아 실행한다.
///
/// frontend 가 plan 을 변형해서 보낼 가능성을 차단하려면 plan 객체에 서명/HMAC 을 붙이는
/// 방식이 이상적이지만 V1 은 frontend 도 같은 프로세스 내라 신뢰. V2 에서 필요 시 강화.
#[tauri::command]
#[instrument(skip(plan), fields(disk_id = %plan.disk.id))]
pub async fn execute_create_partition_dangerous(
    plan: CreatePartitionPlan,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || partition::execute_create_partition(plan))
        .await
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "execute_create_partition 워커 패닉");
            format!("execute_create_partition 작업이 비정상 종료되었습니다: {e}")
        })?
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "execute_create_partition 실패");
            e.to_string()
        })
}

/// 마운트된 파티션의 라벨 변경 plan 을 계산. **read-only**.
#[tauri::command]
#[instrument(skip(new_label))]
pub async fn plan_set_label(
    disk_id: String,
    partition_id: String,
    new_label: String,
) -> Result<SetLabelPlan, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<SetLabelPlan, ParqError> {
        let disk = fetch_disk(&disk_id)?;
        partition::plan_set_label(&disk, &partition_id, new_label)
    })
    .await
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "plan_set_label 워커 패닉");
        format!("plan_set_label 작업이 비정상 종료되었습니다: {e}")
    })?
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "plan_set_label 실패");
        e.to_string()
    })
}

#[tauri::command]
#[instrument(skip(plan), fields(disk_id = %plan.disk.id, partition_id = %plan.partition.id))]
pub async fn execute_set_label_dangerous(plan: SetLabelPlan) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || partition::execute_set_label(plan))
        .await
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "execute_set_label 워커 패닉");
            format!("execute_set_label 작업이 비정상 종료되었습니다: {e}")
        })?
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "execute_set_label 실패");
            e.to_string()
        })
}

/// 파티션 삭제 plan. **read-only**. 마운트된 파티션은 거부.
#[tauri::command]
#[instrument]
pub async fn plan_delete_partition(
    disk_id: String,
    partition_id: String,
) -> Result<DeletePartitionPlan, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<DeletePartitionPlan, ParqError> {
        let disk = fetch_disk(&disk_id)?;
        partition::plan_delete_partition(&disk, &partition_id)
    })
    .await
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "plan_delete_partition 워커 패닉");
        format!("plan_delete_partition 작업이 비정상 종료되었습니다: {e}")
    })?
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "plan_delete_partition 실패");
        e.to_string()
    })
}

#[tauri::command]
#[instrument(skip(plan), fields(disk_id = %plan.disk.id, partition_id = %plan.partition.id))]
pub async fn execute_delete_partition_dangerous(plan: DeletePartitionPlan) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || partition::execute_delete_partition(plan))
        .await
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "execute_delete_partition 워커 패닉");
            format!("execute_delete_partition 작업이 비정상 종료되었습니다: {e}")
        })?
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "execute_delete_partition 실패");
            e.to_string()
        })
}

/// 드라이브 문자 제거 plan. **read-only**. 데이터는 유지됨.
#[tauri::command]
#[instrument]
pub async fn plan_dismount(
    disk_id: String,
    partition_id: String,
) -> Result<DismountPlan, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<DismountPlan, ParqError> {
        let disk = fetch_disk(&disk_id)?;
        partition::plan_dismount(&disk, &partition_id)
    })
    .await
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "plan_dismount 워커 패닉");
        format!("plan_dismount 작업이 비정상 종료되었습니다: {e}")
    })?
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "plan_dismount 실패");
        e.to_string()
    })
}

/// 드라이브 문자 제거는 데이터 유지 메타 작업이지만 일관성을 위해 _dangerous 접미사 유지
/// (모든 write 커맨드는 dangerous 라는 컨벤션을 깨지 않는다).
#[tauri::command]
#[instrument(skip(plan), fields(disk_id = %plan.disk.id, partition_id = %plan.partition.id))]
pub async fn execute_dismount_dangerous(plan: DismountPlan) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || partition::execute_dismount(plan))
        .await
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "execute_dismount 워커 패닉");
            format!("execute_dismount 작업이 비정상 종료되었습니다: {e}")
        })?
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "execute_dismount 실패");
            e.to_string()
        })
}

/// 파티션 리사이즈 가능 범위 조회. **read-only**.
/// 프론트엔드 form 에서 슬라이더 한계 표시용.
#[tauri::command]
#[instrument]
pub async fn get_resize_limits(
    disk_id: String,
    partition_id: String,
) -> Result<ResizeLimits, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<ResizeLimits, ParqError> {
        let disk = fetch_disk(&disk_id)?;
        partition::query_resize_limits(&disk, &partition_id)
    })
    .await
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "get_resize_limits 워커 패닉");
        format!("get_resize_limits 작업이 비정상 종료되었습니다: {e}")
    })?
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "get_resize_limits 실패");
        e.to_string()
    })
}

/// 파티션 리사이즈 plan. **read-only**.
#[tauri::command]
#[instrument]
pub async fn plan_resize_partition(
    disk_id: String,
    partition_id: String,
    new_size_bytes: u64,
) -> Result<ResizePartitionPlan, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<ResizePartitionPlan, ParqError> {
        let disk = fetch_disk(&disk_id)?;
        partition::plan_resize_partition(&disk, &partition_id, new_size_bytes)
    })
    .await
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "plan_resize_partition 워커 패닉");
        format!("plan_resize_partition 작업이 비정상 종료되었습니다: {e}")
    })?
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "plan_resize_partition 실패");
        e.to_string()
    })
}

#[tauri::command]
#[instrument(skip(plan), fields(disk_id = %plan.disk.id, partition_id = %plan.partition.id))]
pub async fn execute_resize_partition_dangerous(
    plan: ResizePartitionPlan,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || partition::execute_resize_partition(plan))
        .await
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "execute_resize_partition 워커 패닉");
            format!("execute_resize_partition 작업이 비정상 종료되었습니다: {e}")
        })?
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "execute_resize_partition 실패");
            e.to_string()
        })
}
