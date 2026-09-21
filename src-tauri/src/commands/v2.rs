// V2 destructive Tauri commands — 파티션 이동. **알파 게이트 뒤에서만.**
//
// docs/v2-charter.md §3-1: 모든 V2 destructive 는 PARQ_ENABLE_V2_DESTRUCTIVE 없이는 거부된다.
// move_engine 이 이를 강제하므로 여기서는 얇게 래핑만 한다. UI 는 v2_destructive_enabled 로
// 게이트 상태를 확인해 이동 UI 노출 여부를 결정한다.
//
// plan/execute 분리는 다른 커맨드와 동일 — plan 으로 미리보기(방향/오프셋/길이) 후 execute.

use serde::Serialize;
use tracing::{error, instrument};

#[cfg(windows)]
use crate::disk;
use crate::ParqError;

/// 프론트엔드로 보내는 이동 plan 미리보기(방향/오프셋/길이). camelCase.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovePlanDto {
    pub disk_number: u32,
    pub src_start_lba: u64,
    pub new_start_lba: u64,
    pub length_sectors: u64,
    pub direction: String,
    pub summary: String,
}

/// 이동 실행 결과. camelCase.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveResultDto {
    pub partition_id: String,
    pub old_start_lba: u64,
    pub new_start_lba: u64,
    pub length_sectors: u64,
    pub sha256: String,
    pub resumed: bool,
}

/// V2 destructive 알파 게이트가 켜져 있는지. UI 가 이동 컨트롤 노출 여부를 결정하는 데 쓴다.
#[tauri::command]
#[instrument]
pub async fn v2_destructive_enabled() -> bool {
    crate::safety::v2_enabled()
}

/// 파티션 이동 plan 미리보기. 알파 게이트 통과 필수(비활성 시 거부). **디스크에 쓰지 않는다.**
#[tauri::command]
#[instrument]
pub async fn plan_move_partition(
    disk_id: String,
    partition_id: String,
    new_start_bytes: u64,
) -> Result<MovePlanDto, String> {
    tauri::async_runtime::spawn_blocking(move || run_plan(&disk_id, &partition_id, new_start_bytes))
        .await
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "plan_move_partition 워커 패닉");
            format!("plan_move_partition 작업이 비정상 종료되었습니다: {e}")
        })?
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "plan_move_partition 실패");
            e.to_string()
        })
}

/// 파티션 이동 실행. **파괴적.** move_engine 이 알파 게이트 + 디스크/파티션 가드 + 인접 무변경
/// 검증 + checkpoint + 라운드트립을 강제한다.
#[tauri::command]
#[instrument]
pub async fn execute_move_partition_dangerous(
    disk_id: String,
    partition_id: String,
    new_start_bytes: u64,
) -> Result<MoveResultDto, String> {
    tauri::async_runtime::spawn_blocking(move || {
        run_execute(&disk_id, &partition_id, new_start_bytes)
    })
    .await
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "execute_move_partition 워커 패닉");
        format!("execute_move_partition 작업이 비정상 종료되었습니다: {e}")
    })?
    .map_err(|e| {
        error!(target: "parq::commands", error = %e, "execute_move_partition 실패");
        e.to_string()
    })
}

#[cfg(windows)]
fn resolve(
    disk_id: &str,
    partition_id: &str,
    new_start_bytes: u64,
) -> Result<(u32, u64, u64, u64), ParqError> {
    let disks = disk::enumerate()?;
    let d = disks
        .iter()
        .find(|d| d.id == disk_id)
        .ok_or_else(|| ParqError::DiskNotFound(disk_id.to_string()))?;
    let p = d
        .partitions
        .iter()
        .find(|p| p.id == partition_id)
        .ok_or_else(|| ParqError::ValidationFailed(format!("파티션 {partition_id} 없음")))?;

    // 섹터 크기: raw geometry 로 조회(관리자 권한 필요 — 이동엔 어차피 필요).
    let rd = crate::raw_io::open_physical_drive_readonly(d.number)?;
    let sector = rd.geometry().logical_sector_bytes as u64;
    if sector == 0 {
        return Err(ParqError::Platform("논리 섹터 크기 0".into()));
    }
    if new_start_bytes % sector != 0 {
        return Err(ParqError::ValidationFailed(format!(
            "새 시작 오프셋({new_start_bytes})이 섹터({sector}) 정렬이 아닙니다"
        )));
    }
    if new_start_bytes < 1024 * 1024 {
        return Err(ParqError::ValidationFailed(
            "새 시작 오프셋은 MBR/부트 예약 영역 보호를 위해 최소 1 MiB 이상이어야 합니다".into(),
        ));
    }
    let src_lba = p.offset_bytes / sector;
    let dst_lba = new_start_bytes / sector;
    let len = p.size_bytes / sector;
    Ok((d.number, src_lba, dst_lba, len))
}

#[cfg(windows)]
fn run_plan(
    disk_id: &str,
    partition_id: &str,
    new_start_bytes: u64,
) -> Result<MovePlanDto, ParqError> {
    use crate::move_engine;
    let (number, src_lba, dst_lba, len) = resolve(disk_id, partition_id, new_start_bytes)?;
    let plan = move_engine::plan_move(number, src_lba, dst_lba, len)?;
    let direction = format!("{:?}", plan.direction);
    Ok(MovePlanDto {
        disk_number: number,
        src_start_lba: src_lba,
        new_start_lba: dst_lba,
        length_sectors: len,
        summary: format!(
            "디스크 {number} 파티션을 LBA {src_lba} → {dst_lba} 로 이동 ({len} sectors, {direction})",
        ),
        direction,
    })
}

#[cfg(not(windows))]
fn run_plan(_: &str, _: &str, _: u64) -> Result<MovePlanDto, ParqError> {
    Err(ParqError::NotImplemented("파티션 이동 (Windows 전용)"))
}

#[cfg(windows)]
fn run_execute(
    disk_id: &str,
    partition_id: &str,
    new_start_bytes: u64,
) -> Result<MoveResultDto, ParqError> {
    use crate::move_engine;
    let (number, src_lba, dst_lba, _len) = resolve(disk_id, partition_id, new_start_bytes)?;
    let ckpt = checkpoint_path(partition_id)?;
    let outcome = move_engine::move_partition(number, src_lba, dst_lba, &ckpt)?;
    Ok(MoveResultDto {
        partition_id: outcome.partition_id,
        old_start_lba: outcome.old_start_lba,
        new_start_lba: outcome.new_start_lba,
        length_sectors: outcome.length_sectors,
        sha256: outcome.data.sha256,
        resumed: outcome.data.resumed,
    })
}

#[cfg(not(windows))]
fn run_execute(_: &str, _: &str, _: u64) -> Result<MoveResultDto, ParqError> {
    Err(ParqError::NotImplemented("파티션 이동 (Windows 전용)"))
}

/// checkpoint 파일 경로. 이동 대상과 **다른 디스크**여야 하므로 시스템 디스크의 LOCALAPPDATA 에.
/// docs/v2-checkpoint-format.md §3. 안정적 이름 → 같은 파티션 재이동 재개 가능.
#[cfg(windows)]
fn checkpoint_path(partition_id: &str) -> Result<std::path::PathBuf, ParqError> {
    let local = std::env::var("LOCALAPPDATA").map_err(|_| {
        ParqError::Platform("LOCALAPPDATA 환경변수가 설정되어 있지 않습니다".into())
    })?;
    let dir = std::path::PathBuf::from(local)
        .join("Parq")
        .join("checkpoints");
    std::fs::create_dir_all(&dir)
        .map_err(|e| ParqError::Platform(format!("checkpoint 디렉토리 생성 실패: {e}")))?;
    let safe: String = partition_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    Ok(dir.join(format!("move-{safe}.json")))
}
