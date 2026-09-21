// 파티션 작업 (생성 / 삭제 / 리사이즈 / 라벨 변경).
//
// 모든 함수는 4단계 패턴을 따른다: plan → validate → preview → execute.
// transaction 모듈을 거치지 않는 쓰기 코드는 PR 거절 사유.
//
// 자세한 안전 모델은 docs/safety-model.md 참고.

use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::disk::{Disk, FileSystemKind, Partition, PartitionStyle};
use crate::platform::powershell;
use crate::transaction::{self, BeginParams};
use crate::{fs as parq_fs, safety, ParqError, Result};

/// 파티션 크기 요청.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "value")]
pub enum SizeRequest {
    /// 정확한 바이트 수.
    Bytes(u64),
    /// 사용 가능한 최대 (PowerShell `-UseMaximumSize`).
    UseMaximum,
}

/// `create_partition` 작업의 plan. 사용자에게 preview 로 보여주고, execute 의 입력이 된다.
///
/// `disk` 는 enumerate 시점의 스냅샷이라 execute 직전 상태와 다를 수 있다 — execute 가 다시
/// 가드를 통과시키므로 안전.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatePartitionPlan {
    pub disk: Disk,
    pub size_request: SizeRequest,
    pub file_system: FileSystemKind,
    pub label: Option<String>,
    /// 디스크가 Raw 면 GPT 로 초기화해야 함을 표시.
    pub initialize_as_gpt: bool,
    /// 사람이 읽을 요약 (트랜잭션 로그 / 미리보기).
    pub summary: String,
}

fn reload_disk(expected: &Disk) -> Result<Disk> {
    let current = crate::disk::enumerate()?
        .into_iter()
        .find(|disk| disk.id == expected.id)
        .ok_or_else(|| {
            ParqError::ValidationFailed(format!(
                "계획 대상 디스크 {} 이 현재 연결된 디스크 목록에 없습니다 — 다시 새로고침하세요",
                expected.id
            ))
        })?;
    if current.number != expected.number
        || current.serial != expected.serial
        || current.size_bytes != expected.size_bytes
        || current.model != expected.model
    {
        return Err(ParqError::ValidationFailed(
            "계획 이후 디스크 식별 정보가 변경되었습니다 — 작업을 중단하고 다시 미리보기하세요"
                .into(),
        ));
    }
    Ok(current)
}

fn require_fresh_plan<T: PartialEq>(submitted: &T, current: &T) -> Result<()> {
    if submitted != current {
        return Err(ParqError::ValidationFailed(
            "미리보기 이후 디스크 또는 파티션 상태가 변경되었습니다 — 새로고침 후 다시 미리보기하세요"
                .into(),
        ));
    }
    Ok(())
}

/// 빈 디스크 또는 free space 가 있는 디스크에 새 파티션 + 포맷을 생성하는 plan 을 만든다.
///
/// **read-only**. 디스크에 어떤 변경도 가하지 않는다.
#[instrument(skip(disk), fields(disk_id = %disk.id, fs = ?file_system))]
pub fn plan_create_partition(
    disk: &Disk,
    size_request: SizeRequest,
    file_system: FileSystemKind,
    label: Option<String>,
) -> Result<CreatePartitionPlan> {
    safety::check_disk_writable(disk)?;

    // V1 은 FAT32 / exFAT / NTFS 만 지원.
    match file_system {
        FileSystemKind::Fat32 | FileSystemKind::ExFat | FileSystemKind::Ntfs => {}
        other => {
            return Err(ParqError::ValidationFailed(format!(
                "V1 은 FAT32 / exFAT / NTFS 만 지원합니다 — 요청: {other:?}"
            )))
        }
    }

    if let Some(ref l) = label {
        parq_fs::validate_label(l, file_system)?;
    }

    let initialize_as_gpt = matches!(disk.partition_style, PartitionStyle::Raw);

    if matches!(disk.partition_style, PartitionStyle::Raw) && !disk.partitions.is_empty() {
        // Raw 인데 partitions 가 있으면 일관성 깨진 상태. 거부.
        return Err(ParqError::ValidationFailed(
            "디스크가 Raw 인데 파티션이 보고되어 일관성이 깨졌습니다 — 다시 enumerate 후 시도하세요"
                .into(),
        ));
    }

    if let SizeRequest::Bytes(b) = size_request {
        if b == 0 {
            return Err(ParqError::ValidationFailed("파티션 크기가 0 입니다".into()));
        }
        if b > disk.size_bytes {
            return Err(ParqError::ValidationFailed(format!(
                "요청 크기 ({b} B) 가 디스크 크기 ({} B) 를 초과합니다",
                disk.size_bytes
            )));
        }
    }

    let size_human = match size_request {
        SizeRequest::Bytes(b) => format!("{b} B"),
        SizeRequest::UseMaximum => "최대 가용".into(),
    };
    let label_human = label.clone().unwrap_or_else(|| "(라벨 없음)".into());
    let summary = format!(
        "디스크 #{} ({}, {}): {} 새 파티션 + {:?} 포맷, 라벨={}{}",
        disk.number,
        disk.model,
        format_bytes(disk.size_bytes),
        size_human,
        file_system,
        label_human,
        if initialize_as_gpt {
            " (디스크 GPT 초기화 포함)"
        } else {
            ""
        }
    );

    Ok(CreatePartitionPlan {
        disk: disk.clone(),
        size_request,
        file_system,
        label,
        initialize_as_gpt,
        summary,
    })
}

/// `plan_create_partition` 의 결과를 트랜잭션 안에서 실행한다.
///
/// 단계:
/// 1. (Raw 디스크인 경우) Initialize-Disk -PartitionStyle GPT
/// 2. New-Partition -DiskNumber N -Size B|UseMaximumSize -AssignDriveLetter
/// 3. fs::format_volume(드라이브 문자, FS, 라벨)
///
/// 어느 단계에서든 실패하면 transaction 이 failed 로 마감되고 에러가 전파된다.
/// 단, 부분 성공 (예: 1·2 성공, 3 실패) 시 자동 복구는 수행하지 않는다 — 단순 fwd-only
/// 트랜잭션. 사용자는 로그를 보고 수동 복구하거나 다시 시도한다. (V2 에서 자동 cleanup 검토)
#[instrument(skip(plan), fields(disk_id = %plan.disk.id))]
pub fn execute_create_partition(plan: CreatePartitionPlan) -> Result<()> {
    let current_disk = reload_disk(&plan.disk)?;
    let current_plan = plan_create_partition(
        &current_disk,
        plan.size_request,
        plan.file_system,
        plan.label.clone(),
    )?;
    require_fresh_plan(&plan, &current_plan)?;
    let plan = current_plan;
    // 진입 시 한 번 더 디스크 가드 — plan 시점 이후 환경이 바뀌었을 수 있음.
    safety::check_disk_writable(&plan.disk)?;

    let disk_summary = format!(
        "디스크 #{} {} ({}, {:?})",
        plan.disk.number,
        plan.disk.model,
        format_bytes(plan.disk.size_bytes),
        plan.disk.bus_type,
    );
    let mut txn = transaction::begin(BeginParams {
        operation: "create_partition",
        disk_id: &plan.disk.id,
        disk_summary: &disk_summary,
        plan_summary: &plan.summary,
    })?;

    let outcome = run_create_partition(&mut txn, &plan);
    match outcome {
        Ok(()) => txn.commit(),
        Err(e) => {
            let reason = e.to_string();
            // 실패 로그 마감 자체가 실패해도 원래 에러를 우선시한다.
            if let Err(re) = txn.fail(&reason) {
                tracing::warn!(
                    target: "parq::partition",
                    finalize_error = %re,
                    "실패 로그 마감 실패"
                );
            }
            Err(e)
        }
    }
}

fn run_create_partition(
    txn: &mut transaction::Transaction,
    plan: &CreatePartitionPlan,
) -> Result<()> {
    if plan.initialize_as_gpt {
        let disk_number = plan.disk.number;
        txn.run_step("initialize_disk_gpt", || {
            let script = format!(
                "Initialize-Disk -Number {disk_number} -PartitionStyle GPT -Confirm:$false"
            );
            powershell::run_command(&script).map(|_| ())
        })?;
    }

    let drive_letter = {
        let disk_number = plan.disk.number;
        let size_arg = match plan.size_request {
            SizeRequest::Bytes(b) => format!("-Size {b}"),
            SizeRequest::UseMaximum => "-UseMaximumSize".to_string(),
        };
        txn.run_step("new_partition", move || {
            // Out-String 으로 강제하지 않고 단일 char 만 출력해서 trim 으로 추출.
            let script = format!(
                "$p = New-Partition -DiskNumber {disk_number} {size_arg} -AssignDriveLetter; \
                 [string]$p.DriveLetter"
            );
            let out = powershell::run_command(&script)?;
            let letter = out.stdout.trim().to_string();
            if letter.len() != 1 || !letter.chars().next().unwrap().is_ascii_alphabetic() {
                return Err(ParqError::Platform(format!(
                    "New-Partition 이 유효한 드라이브 문자를 반환하지 않았습니다: {letter:?}"
                )));
            }
            Ok(letter)
        })?
    };

    let fs = plan.file_system;
    let label = plan.label.clone();
    let dl = drive_letter.clone();
    txn.run_step("format_volume", move || {
        parq_fs::format_volume(&dl, fs, label.as_deref())
    })?;

    Ok(())
}

/// `set_label` 작업의 plan. 라벨은 메타데이터 변경이므로 마운트 상태에서도 허용.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetLabelPlan {
    pub disk: Disk,
    pub partition: Partition,
    pub new_label: String,
    pub summary: String,
}

/// 마운트된 파티션의 라벨을 새 값으로 바꾸는 plan 을 만든다. **read-only**.
#[instrument(skip(disk, new_label), fields(disk_id = %disk.id, partition_id = partition_id))]
pub fn plan_set_label(disk: &Disk, partition_id: &str, new_label: String) -> Result<SetLabelPlan> {
    let partition = disk
        .partitions
        .iter()
        .find(|p| p.id == partition_id)
        .ok_or_else(|| {
            ParqError::ValidationFailed(format!("파티션 {partition_id} 을 찾을 수 없습니다"))
        })?;

    safety::check_partition_metadata_writable(disk, partition)?;

    if partition.drive_letter.is_none() {
        return Err(ParqError::ValidationFailed(format!(
            "파티션 {partition_id} 에 드라이브 문자가 없습니다 — V1 라벨 변경은 마운트된 \
             볼륨에만 가능합니다 (Set-Volume -DriveLetter)"
        )));
    }

    parq_fs::validate_label(&new_label, partition.file_system)?;

    let summary = format!(
        "디스크 #{} ({}, {}): 파티션 #{} ({}:) 라벨 \"{}\" → \"{}\"",
        disk.number,
        disk.model,
        format_bytes(disk.size_bytes),
        partition.index,
        partition.drive_letter.as_deref().unwrap_or(""),
        partition.label.as_deref().unwrap_or(""),
        new_label,
    );

    Ok(SetLabelPlan {
        disk: disk.clone(),
        partition: partition.clone(),
        new_label,
        summary,
    })
}

/// `plan_set_label` 결과를 트랜잭션 안에서 실행한다.
#[instrument(skip(plan), fields(disk_id = %plan.disk.id, partition_id = %plan.partition.id))]
pub fn execute_set_label(plan: SetLabelPlan) -> Result<()> {
    let current_disk = reload_disk(&plan.disk)?;
    let current_plan = plan_set_label(&current_disk, &plan.partition.id, plan.new_label.clone())?;
    require_fresh_plan(&plan, &current_plan)?;
    let plan = current_plan;
    safety::check_partition_metadata_writable(&plan.disk, &plan.partition)?;
    let drive_letter =
        plan.partition.drive_letter.clone().ok_or_else(|| {
            ParqError::ValidationFailed("파티션에 드라이브 문자가 없습니다".into())
        })?;

    let disk_summary = format!(
        "디스크 #{} {} ({}, {:?})",
        plan.disk.number,
        plan.disk.model,
        format_bytes(plan.disk.size_bytes),
        plan.disk.bus_type,
    );
    let mut txn = transaction::begin(BeginParams {
        operation: "set_label",
        disk_id: &plan.disk.id,
        disk_summary: &disk_summary,
        plan_summary: &plan.summary,
    })?;

    let fs = plan.partition.file_system;
    let new_label = plan.new_label.clone();
    let outcome = txn.run_step("set_volume_label", move || {
        parq_fs::set_volume_label(&drive_letter, fs, &new_label)
    });

    match outcome {
        Ok(()) => txn.commit(),
        Err(e) => {
            let reason = e.to_string();
            if let Err(re) = txn.fail(&reason) {
                tracing::warn!(
                    target: "parq::partition",
                    finalize_error = %re,
                    "실패 로그 마감 실패"
                );
            }
            Err(e)
        }
    }
}

/// 리사이즈 가능 범위 (PowerShell `Get-PartitionSupportedSize` 의 결과 + 현재 크기).
///
/// V1 은 NTFS 만 지원. min 은 immovable 파일 (MFT, 페이지파일, 하이버네이션 등) 위치에 의해
/// Windows 가 결정하는 값. max 는 현재 위치 기준 뒤쪽 인접 미할당까지 포함한 최대 크기.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResizeLimits {
    pub current_bytes: u64,
    pub min_bytes: u64,
    pub max_bytes: u64,
}

/// 파티션의 리사이즈 한계를 조회한다. **read-only** (Windows 에 쿼리만 보냄, 변경 없음).
#[instrument(skip(disk), fields(disk_id = %disk.id, partition_id = partition_id))]
pub fn query_resize_limits(disk: &Disk, partition_id: &str) -> Result<ResizeLimits> {
    let partition = disk
        .partitions
        .iter()
        .find(|p| p.id == partition_id)
        .ok_or_else(|| {
            ParqError::ValidationFailed(format!("파티션 {partition_id} 을 찾을 수 없습니다"))
        })?;

    safety::check_partition_resize_writable(disk, partition)?;
    require_resizable_fs(partition.file_system)?;

    let supported = query_supported_size(disk.number, partition.index)?;
    Ok(ResizeLimits {
        current_bytes: partition.size_bytes,
        min_bytes: supported.size_min,
        max_bytes: supported.size_max,
    })
}

/// `resize_partition` 작업의 plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResizePartitionPlan {
    pub disk: Disk,
    pub partition: Partition,
    pub new_size_bytes: u64,
    pub current_size_bytes: u64,
    pub min_supported_bytes: u64,
    pub max_supported_bytes: u64,
    pub summary: String,
}

/// NTFS 파티션 리사이즈 plan. **read-only** (Windows 쿼리만, 디스크 변경 없음).
#[instrument(skip(disk), fields(disk_id = %disk.id, partition_id = partition_id, new_size = new_size_bytes))]
pub fn plan_resize_partition(
    disk: &Disk,
    partition_id: &str,
    new_size_bytes: u64,
) -> Result<ResizePartitionPlan> {
    let partition = disk
        .partitions
        .iter()
        .find(|p| p.id == partition_id)
        .ok_or_else(|| {
            ParqError::ValidationFailed(format!("파티션 {partition_id} 을 찾을 수 없습니다"))
        })?;

    safety::check_partition_resize_writable(disk, partition)?;
    require_resizable_fs(partition.file_system)?;

    let supported = query_supported_size(disk.number, partition.index)?;
    let limits = ResizeLimits {
        current_bytes: partition.size_bytes,
        min_bytes: supported.size_min,
        max_bytes: supported.size_max,
    };
    validate_resize_size(new_size_bytes, &limits)?;

    let direction = if new_size_bytes > partition.size_bytes {
        "확장"
    } else {
        "축소"
    };
    let summary = format!(
        "디스크 #{} ({}, {}): 파티션 #{} ({}) 크기 {} → {} [{}] · 가능 범위 [{} ~ {}]",
        disk.number,
        disk.model,
        format_bytes(disk.size_bytes),
        partition.index,
        partition
            .drive_letter
            .as_deref()
            .map(|d| format!("{d}:"))
            .unwrap_or_else(|| "마운트 없음".into()),
        format_bytes(partition.size_bytes),
        format_bytes(new_size_bytes),
        direction,
        format_bytes(limits.min_bytes),
        format_bytes(limits.max_bytes),
    );

    Ok(ResizePartitionPlan {
        disk: disk.clone(),
        partition: partition.clone(),
        new_size_bytes,
        current_size_bytes: partition.size_bytes,
        min_supported_bytes: limits.min_bytes,
        max_supported_bytes: limits.max_bytes,
        summary,
    })
}

/// `plan_resize_partition` 결과를 트랜잭션 안에서 실행한다.
///
/// `resize_partition`으로 `Resize-Partition`을 호출한 뒤 `verify_resize`에서 실제 크기를
/// 다시 열거해 요청값과 정확히 일치하는지 확인한다.
#[instrument(skip(plan), fields(disk_id = %plan.disk.id, partition_id = %plan.partition.id))]
pub fn execute_resize_partition(plan: ResizePartitionPlan) -> Result<()> {
    let current_disk = reload_disk(&plan.disk)?;
    let current_plan =
        plan_resize_partition(&current_disk, &plan.partition.id, plan.new_size_bytes)?;
    require_fresh_plan(&plan, &current_plan)?;
    let plan = current_plan;
    safety::check_partition_resize_writable(&plan.disk, &plan.partition)?;
    require_resizable_fs(plan.partition.file_system)?;

    let disk_summary = format!(
        "디스크 #{} {} ({}, {:?})",
        plan.disk.number,
        plan.disk.model,
        format_bytes(plan.disk.size_bytes),
        plan.disk.bus_type,
    );
    let mut txn = transaction::begin(BeginParams {
        operation: "resize_partition",
        disk_id: &plan.disk.id,
        disk_summary: &disk_summary,
        plan_summary: &plan.summary,
    })?;

    let disk_number = plan.disk.number;
    let partition_number = plan.partition.index;
    let new_size = plan.new_size_bytes;
    let expected_disk = plan.disk.clone();
    let expected_partition_id = plan.partition.id.clone();
    let outcome = (|| {
        txn.run_step("resize_partition", move || {
            let script = format!(
                "Resize-Partition -DiskNumber {disk_number} -PartitionNumber {partition_number} \
                 -Size {new_size} -Confirm:$false"
            );
            powershell::run_command(&script).map(|_| ())
        })?;
        txn.run_step("verify_resize", move || {
            let current_disk = reload_disk(&expected_disk)?;
            let current_partition = current_disk
                .partitions
                .iter()
                .find(|partition| partition.id == expected_partition_id)
                .ok_or_else(|| {
                    ParqError::ValidationFailed(
                        "리사이즈 후 대상 파티션을 다시 찾을 수 없습니다".into(),
                    )
                })?;
            if current_partition.size_bytes != new_size {
                return Err(ParqError::ValidationFailed(format!(
                    "리사이즈 후 크기 검증 실패: actual={}, expected={new_size}",
                    current_partition.size_bytes
                )));
            }
            Ok(())
        })
    })();

    match outcome {
        Ok(()) => txn.commit(),
        Err(e) => {
            let reason = e.to_string();
            if let Err(re) = txn.fail(&reason) {
                tracing::warn!(
                    target: "parq::partition",
                    finalize_error = %re,
                    "실패 로그 마감 실패"
                );
            }
            Err(e)
        }
    }
}

fn require_resizable_fs(fs: FileSystemKind) -> Result<()> {
    match fs {
        FileSystemKind::Ntfs => Ok(()),
        other => Err(ParqError::ValidationFailed(format!(
            "V1 은 NTFS 파티션만 리사이즈할 수 있습니다 — 현재 파일시스템: {other:?}. \
             FAT32 / exFAT 는 Windows 가 리사이즈를 지원하지 않습니다."
        ))),
    }
}

fn validate_resize_size(new_size_bytes: u64, limits: &ResizeLimits) -> Result<()> {
    if new_size_bytes == limits.current_bytes {
        return Err(ParqError::ValidationFailed(
            "변경 없음 — 새 크기가 현재 크기와 같습니다".into(),
        ));
    }
    if new_size_bytes < limits.min_bytes {
        return Err(ParqError::ValidationFailed(format!(
            "요청 크기 ({} B) 가 Windows 가 허용하는 최소 크기 ({} B) 보다 작습니다 — \
             immovable 파일 (MFT / 페이지파일 / 하이버네이션 등) 위치 때문",
            new_size_bytes, limits.min_bytes
        )));
    }
    if new_size_bytes > limits.max_bytes {
        return Err(ParqError::ValidationFailed(format!(
            "요청 크기 ({} B) 가 최대 가능 크기 ({} B) 를 초과합니다 — \
             뒤쪽에 인접한 미할당 공간이 부족합니다",
            new_size_bytes, limits.max_bytes
        )));
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct PsSupportedSize {
    #[serde(rename = "SizeMin")]
    size_min: u64,
    #[serde(rename = "SizeMax")]
    size_max: u64,
}

fn query_supported_size(disk_number: u32, partition_number: u32) -> Result<PsSupportedSize> {
    let script = format!(
        "Get-PartitionSupportedSize -DiskNumber {disk_number} -PartitionNumber \
         {partition_number} | Select-Object SizeMin, SizeMax | ConvertTo-Json -Compress"
    );
    powershell::run_json(&script)
}

/// `dismount` 작업의 plan. 드라이브 문자만 제거 — 데이터는 그대로 유지된다.
///
/// destructive 작업 (삭제 / 포맷) 의 사전 단계로 자주 쓰인다 — V1 destructive 가드가
/// 마운트된 파티션을 거부하므로 사용자가 먼저 마운트 해제하는 길.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DismountPlan {
    pub disk: Disk,
    pub partition: Partition,
    pub drive_letter: String,
    pub summary: String,
}

/// 마운트된 파티션의 드라이브 문자를 제거하는 plan. **read-only**.
///
/// 라벨 변경과 동일하게 `check_partition_metadata_writable` 가드 사용 — 데이터를 직접
/// 손상시키지 않으므로 마운트 상태에서도 허용.
#[instrument(skip(disk), fields(disk_id = %disk.id, partition_id = partition_id))]
pub fn plan_dismount(disk: &Disk, partition_id: &str) -> Result<DismountPlan> {
    let partition = disk
        .partitions
        .iter()
        .find(|p| p.id == partition_id)
        .ok_or_else(|| {
            ParqError::ValidationFailed(format!("파티션 {partition_id} 을 찾을 수 없습니다"))
        })?;

    safety::check_partition_metadata_writable(disk, partition)?;

    let drive_letter = partition.drive_letter.clone().ok_or_else(|| {
        ParqError::ValidationFailed(format!(
            "파티션 {partition_id} 에 드라이브 문자가 없습니다 (이미 마운트 해제됨)"
        ))
    })?;

    let summary = format!(
        "디스크 #{} ({}, {}): 파티션 #{} 드라이브 문자 \"{}:\" 제거 — 데이터는 유지됨",
        disk.number,
        disk.model,
        format_bytes(disk.size_bytes),
        partition.index,
        drive_letter,
    );

    Ok(DismountPlan {
        disk: disk.clone(),
        partition: partition.clone(),
        drive_letter,
        summary,
    })
}

/// `plan_dismount` 결과를 트랜잭션 안에서 실행한다.
#[instrument(skip(plan), fields(disk_id = %plan.disk.id, partition_id = %plan.partition.id))]
pub fn execute_dismount(plan: DismountPlan) -> Result<()> {
    let current_disk = reload_disk(&plan.disk)?;
    let current_plan = plan_dismount(&current_disk, &plan.partition.id)?;
    require_fresh_plan(&plan, &current_plan)?;
    let plan = current_plan;
    safety::check_partition_metadata_writable(&plan.disk, &plan.partition)?;

    let disk_summary = format!(
        "디스크 #{} {} ({}, {:?})",
        plan.disk.number,
        plan.disk.model,
        format_bytes(plan.disk.size_bytes),
        plan.disk.bus_type,
    );
    let mut txn = transaction::begin(BeginParams {
        operation: "dismount",
        disk_id: &plan.disk.id,
        disk_summary: &disk_summary,
        plan_summary: &plan.summary,
    })?;

    let disk_number = plan.disk.number;
    let partition_number = plan.partition.index;
    let dl = plan.drive_letter.clone();
    let outcome = txn.run_step("remove_partition_access_path", move || {
        // AccessPath 는 "X:\" 형태로 전달.
        let script = format!(
            "Remove-PartitionAccessPath -DiskNumber {disk_number} -PartitionNumber \
             {partition_number} -AccessPath {} -Confirm:$false",
            powershell::quote_single(&format!("{dl}:\\"))
        );
        powershell::run_command(&script).map(|_| ())
    });

    match outcome {
        Ok(()) => txn.commit(),
        Err(e) => {
            let reason = e.to_string();
            if let Err(re) = txn.fail(&reason) {
                tracing::warn!(
                    target: "parq::partition",
                    finalize_error = %re,
                    "실패 로그 마감 실패"
                );
            }
            Err(e)
        }
    }
}

/// `delete_partition` 작업의 plan. 데이터 영구 손실이 발생하는 작업이라 가장 강한 가드 적용.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletePartitionPlan {
    pub disk: Disk,
    pub partition: Partition,
    pub summary: String,
}

/// 파티션 삭제 plan. **read-only**.
///
/// `check_partition_destructive` 를 통과해야 하므로 마운트된 파티션은 거부된다 — 사용자가
/// 먼저 드라이브 문자를 제거하거나 볼륨을 dismount 해야 한다.
#[instrument(skip(disk), fields(disk_id = %disk.id, partition_id = partition_id))]
pub fn plan_delete_partition(disk: &Disk, partition_id: &str) -> Result<DeletePartitionPlan> {
    let partition = disk
        .partitions
        .iter()
        .find(|p| p.id == partition_id)
        .ok_or_else(|| {
            ParqError::ValidationFailed(format!("파티션 {partition_id} 을 찾을 수 없습니다"))
        })?;

    safety::check_partition_destructive(disk, partition)?;

    let summary = format!(
        "디스크 #{} ({}, {}): 파티션 #{} 삭제 — 크기 {}, 라벨=\"{}\", FS={:?} \
         · 이 파티션의 데이터는 영구히 사라집니다",
        disk.number,
        disk.model,
        format_bytes(disk.size_bytes),
        partition.index,
        format_bytes(partition.size_bytes),
        partition.label.as_deref().unwrap_or(""),
        partition.file_system,
    );

    Ok(DeletePartitionPlan {
        disk: disk.clone(),
        partition: partition.clone(),
        summary,
    })
}

/// `plan_delete_partition` 결과를 트랜잭션 안에서 실행한다.
///
/// 단일 step `remove_partition` — `Remove-Partition` cmdlet 호출. 부분 실패 자동 cleanup
/// 없음 (단일 step 이라 의미 없음). PowerShell 이 거부하면 트랜잭션 failed 로 마감.
#[instrument(skip(plan), fields(disk_id = %plan.disk.id, partition_id = %plan.partition.id))]
pub fn execute_delete_partition(plan: DeletePartitionPlan) -> Result<()> {
    let current_disk = reload_disk(&plan.disk)?;
    let current_plan = plan_delete_partition(&current_disk, &plan.partition.id)?;
    require_fresh_plan(&plan, &current_plan)?;
    let plan = current_plan;
    safety::check_partition_destructive(&plan.disk, &plan.partition)?;

    let disk_summary = format!(
        "디스크 #{} {} ({}, {:?})",
        plan.disk.number,
        plan.disk.model,
        format_bytes(plan.disk.size_bytes),
        plan.disk.bus_type,
    );
    let mut txn = transaction::begin(BeginParams {
        operation: "delete_partition",
        disk_id: &plan.disk.id,
        disk_summary: &disk_summary,
        plan_summary: &plan.summary,
    })?;

    let disk_number = plan.disk.number;
    let partition_number = plan.partition.index;
    let outcome = txn.run_step("remove_partition", move || {
        let script = format!(
            "Remove-Partition -DiskNumber {disk_number} -PartitionNumber {partition_number} \
             -Confirm:$false"
        );
        powershell::run_command(&script).map(|_| ())
    });

    match outcome {
        Ok(()) => txn.commit(),
        Err(e) => {
            let reason = e.to_string();
            if let Err(re) = txn.fail(&reason) {
                tracing::warn!(
                    target: "parq::partition",
                    finalize_error = %re,
                    "실패 로그 마감 실패"
                );
            }
            Err(e)
        }
    }
}

/// 사람이 읽기 좋은 바이트 표기 (디스크 크기는 SI base-10 관행 — `lsblk`, `Get-Disk` 와 일치).
fn format_bytes(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = b as f64;
    let mut idx = 0;
    while value >= 1000.0 && idx + 1 < UNITS.len() {
        value /= 1000.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{b} B")
    } else {
        format!("{value:.1} {}", UNITS[idx])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::{BitLockerStatus, BusType, FileSystemKind, PartitionStyle};

    fn raw_usb_disk() -> Disk {
        Disk {
            id: "test-usb#1".into(),
            number: 1,
            model: "Test USB".into(),
            serial: Some("SN".into()),
            size_bytes: 32_000_000_000,
            bus_type: BusType::Usb,
            partition_style: PartitionStyle::Raw,
            is_removable: true,
            is_system: false,
            is_read_only: false,
            partitions: vec![],
            is_writable_v1: false,
        }
    }

    #[test]
    fn plan_creates_summary_for_raw_disk_with_gpt_init() {
        let disk = raw_usb_disk();
        let plan = plan_create_partition(
            &disk,
            SizeRequest::Bytes(1_000_000_000),
            FileSystemKind::Fat32,
            Some("MYUSB".into()),
        )
        .expect("plan");
        assert!(plan.initialize_as_gpt, "Raw 디스크는 GPT 초기화 포함");
        assert!(plan.summary.contains("Fat32") || plan.summary.contains("FAT32"));
        assert!(plan.summary.contains("MYUSB"));
    }

    #[test]
    fn plan_no_init_for_gpt_disk() {
        let mut disk = raw_usb_disk();
        disk.partition_style = PartitionStyle::Gpt;
        let plan =
            plan_create_partition(&disk, SizeRequest::UseMaximum, FileSystemKind::Ntfs, None)
                .expect("plan");
        assert!(!plan.initialize_as_gpt);
    }

    #[test]
    fn stale_or_modified_plan_is_rejected() {
        let disk = raw_usb_disk();
        let plan = plan_create_partition(
            &disk,
            SizeRequest::Bytes(1_000_000_000),
            FileSystemKind::Ntfs,
            Some("DATA".into()),
        )
        .expect("plan");
        assert!(require_fresh_plan(&plan, &plan).is_ok());

        let mut changed = plan.clone();
        changed.disk.size_bytes += 512;
        let err = require_fresh_plan(&plan, &changed).unwrap_err();
        assert!(err.to_string().contains("상태가 변경"));
    }

    #[test]
    fn plan_rejects_unsupported_fs() {
        let disk = raw_usb_disk();
        let err = plan_create_partition(&disk, SizeRequest::UseMaximum, FileSystemKind::ReFs, None)
            .unwrap_err();
        assert!(matches!(err, ParqError::ValidationFailed(_)));
    }

    #[test]
    fn plan_rejects_zero_size() {
        let disk = raw_usb_disk();
        let err = plan_create_partition(&disk, SizeRequest::Bytes(0), FileSystemKind::Fat32, None)
            .unwrap_err();
        assert!(err.to_string().contains("0"));
    }

    #[test]
    fn plan_rejects_oversized_partition() {
        let disk = raw_usb_disk();
        let err = plan_create_partition(
            &disk,
            SizeRequest::Bytes(disk.size_bytes + 1),
            FileSystemKind::Fat32,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("초과"));
    }

    #[test]
    fn plan_rejects_label_violation() {
        let disk = raw_usb_disk();
        let err = plan_create_partition(
            &disk,
            SizeRequest::UseMaximum,
            FileSystemKind::Fat32,
            Some("123456789012".into()), // 12 chars > FAT32 max 11
        )
        .unwrap_err();
        assert!(err.to_string().contains("라벨"));
    }

    #[test]
    fn plan_blocked_by_safety_for_internal_disk() {
        let _guard = crate::test_support::ENV_LOCK.lock().unwrap();
        std::env::remove_var("PARQ_DEV_ALLOW_INTERNAL_DISKS");
        let mut disk = raw_usb_disk();
        disk.bus_type = BusType::Nvme;
        disk.is_removable = false;
        let err =
            plan_create_partition(&disk, SizeRequest::UseMaximum, FileSystemKind::Fat32, None)
                .unwrap_err();
        assert!(err.to_string().contains("외장 미디어"));
    }

    fn mounted_fat32_partition() -> Partition {
        Partition {
            id: "test-usb#1-part2".into(),
            index: 2,
            offset_bytes: 16_777_216,
            size_bytes: 1_000_000_000,
            drive_letter: Some("E".into()),
            label: Some("OLD-LABEL".into()),
            file_system: FileSystemKind::Fat32,
            is_boot: false,
            is_system: false,
            is_hidden: false,
            bitlocker_status: BitLockerStatus::NotEncrypted,
            is_in_use: true,
        }
    }

    fn usb_disk_with_partition() -> Disk {
        let mut d = raw_usb_disk();
        d.partition_style = PartitionStyle::Gpt;
        d.partitions = vec![mounted_fat32_partition()];
        d
    }

    #[test]
    fn plan_set_label_basic() {
        let disk = usb_disk_with_partition();
        let plan = plan_set_label(&disk, "test-usb#1-part2", "NEW-LABEL".into()).expect("plan");
        assert_eq!(plan.new_label, "NEW-LABEL");
        assert!(plan.summary.contains("OLD-LABEL"));
        assert!(plan.summary.contains("NEW-LABEL"));
    }

    #[test]
    fn plan_set_label_rejects_unknown_partition() {
        let disk = usb_disk_with_partition();
        let err = plan_set_label(&disk, "no-such-id", "X".into()).unwrap_err();
        assert!(err.to_string().contains("찾을 수 없"));
    }

    #[test]
    fn plan_set_label_rejects_unmounted_partition() {
        let mut disk = usb_disk_with_partition();
        disk.partitions[0].drive_letter = None;
        let err = plan_set_label(&disk, "test-usb#1-part2", "NEW".into()).unwrap_err();
        assert!(err.to_string().contains("드라이브 문자"));
    }

    #[test]
    fn plan_set_label_rejects_invalid_label() {
        let disk = usb_disk_with_partition();
        // FAT32 max 11 chars
        let err =
            plan_set_label(&disk, "test-usb#1-part2", "TOO-LONG-FOR-FAT32".into()).unwrap_err();
        assert!(err.to_string().contains("라벨"));
    }

    #[test]
    fn plan_set_label_rejects_boot_partition() {
        let mut disk = usb_disk_with_partition();
        disk.partitions[0].is_boot = true;
        let err = plan_set_label(&disk, "test-usb#1-part2", "X".into()).unwrap_err();
        assert!(matches!(err, ParqError::SystemPartitionProtected(_)));
    }

    #[test]
    fn plan_dismount_basic() {
        let disk = usb_disk_with_partition();
        let plan = plan_dismount(&disk, "test-usb#1-part2").expect("plan");
        assert_eq!(plan.drive_letter, "E");
        assert!(plan.summary.contains("E:"));
    }

    #[test]
    fn plan_dismount_rejects_unmounted() {
        let mut disk = usb_disk_with_partition();
        disk.partitions[0].drive_letter = None;
        disk.partitions[0].is_in_use = false;
        let err = plan_dismount(&disk, "test-usb#1-part2").unwrap_err();
        assert!(err.to_string().contains("이미"));
    }

    #[test]
    fn plan_dismount_rejects_boot_partition() {
        let mut disk = usb_disk_with_partition();
        disk.partitions[0].is_boot = true;
        let err = plan_dismount(&disk, "test-usb#1-part2").unwrap_err();
        assert!(matches!(err, ParqError::SystemPartitionProtected(_)));
    }

    #[test]
    fn plan_delete_basic_unmounted() {
        let mut disk = usb_disk_with_partition();
        disk.partitions[0].drive_letter = None;
        disk.partitions[0].is_in_use = false;
        let plan = plan_delete_partition(&disk, "test-usb#1-part2").expect("plan");
        assert!(plan.summary.contains("영구히"));
        assert_eq!(plan.partition.index, 2);
    }

    #[test]
    fn plan_delete_rejects_mounted() {
        let disk = usb_disk_with_partition(); // is_in_use = true, drive_letter = E
        let err = plan_delete_partition(&disk, "test-usb#1-part2").unwrap_err();
        assert!(err.to_string().contains("마운트"));
    }

    #[test]
    fn plan_delete_rejects_boot_partition() {
        let mut disk = usb_disk_with_partition();
        disk.partitions[0].is_boot = true;
        disk.partitions[0].is_in_use = false;
        disk.partitions[0].drive_letter = None;
        let err = plan_delete_partition(&disk, "test-usb#1-part2").unwrap_err();
        assert!(matches!(err, ParqError::SystemPartitionProtected(_)));
    }

    #[test]
    fn plan_delete_rejects_unknown_partition() {
        let disk = usb_disk_with_partition();
        let err = plan_delete_partition(&disk, "no-such-id").unwrap_err();
        assert!(err.to_string().contains("찾을 수 없"));
    }

    #[test]
    fn require_resizable_fs_ntfs_only() {
        assert!(require_resizable_fs(FileSystemKind::Ntfs).is_ok());
        for fs in [
            FileSystemKind::Fat32,
            FileSystemKind::ExFat,
            FileSystemKind::ReFs,
            FileSystemKind::Efi,
            FileSystemKind::Unknown,
        ] {
            let err = require_resizable_fs(fs).unwrap_err();
            assert!(err.to_string().contains("NTFS"));
        }
    }

    #[test]
    fn validate_resize_rejects_no_change() {
        let limits = ResizeLimits {
            current_bytes: 1_000_000_000,
            min_bytes: 100_000_000,
            max_bytes: 2_000_000_000,
        };
        let err = validate_resize_size(1_000_000_000, &limits).unwrap_err();
        assert!(err.to_string().contains("변경 없음"));
    }

    #[test]
    fn validate_resize_rejects_below_min() {
        let limits = ResizeLimits {
            current_bytes: 1_000_000_000,
            min_bytes: 500_000_000,
            max_bytes: 2_000_000_000,
        };
        let err = validate_resize_size(100_000_000, &limits).unwrap_err();
        assert!(err.to_string().contains("최소"));
    }

    #[test]
    fn validate_resize_rejects_above_max() {
        let limits = ResizeLimits {
            current_bytes: 1_000_000_000,
            min_bytes: 100_000_000,
            max_bytes: 2_000_000_000,
        };
        let err = validate_resize_size(3_000_000_000, &limits).unwrap_err();
        assert!(err.to_string().contains("최대"));
    }

    #[test]
    fn validate_resize_accepts_within_range() {
        let limits = ResizeLimits {
            current_bytes: 1_000_000_000,
            min_bytes: 100_000_000,
            max_bytes: 2_000_000_000,
        };
        assert!(validate_resize_size(1_500_000_000, &limits).is_ok());
        assert!(validate_resize_size(500_000_000, &limits).is_ok());
        // 경계값
        assert!(validate_resize_size(100_000_000, &limits).is_ok());
        assert!(validate_resize_size(2_000_000_000, &limits).is_ok());
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_000), "1.0 KB");
        assert_eq!(format_bytes(1_000_000), "1.0 MB");
        assert_eq!(format_bytes(10_000_000_000), "10.0 GB");
    }
}
