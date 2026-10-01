//! V2 파티션 이동 엔진 (Phase 3). **파괴적.**
//!
//! `docs/v2-move-algorithm.md`(방향/겹침) + `docs/v2-checkpoint-format.md`(write 순서/복구)를 구현한다.
//! src 영역을 dst 영역으로 raw 청크 복사하고, checkpoint 로 중단-재개를 지원하며, 이동 후
//! SHA256 라운드트립(charter §3-7)을 검증한다.
//!
//! 안전: `raw_io::write::open_writable` 가 알파 게이트(`PARQ_ENABLE_V2_DESTRUCTIVE`) + 디스크
//! 가드(시스템 디스크 차단)를 강제한다. 자동 테스트는 VHD 에서만(charter §3-3).
//!
//! ⚠ 현재 범위: **영역(region) 이동 + checkpoint + 라운드트립**. 파티션 테이블 갱신과 인접 파티션
//! 무변경 검증(charter §3-6)은 이 위에 얹는 다음 계층(별도 작업). 즉 이 엔진만으로는 아직 "완결된
//! 파티션 이동"이 아니라, 그 핵심 데이터 이동 프리미티브다.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, instrument, warn};

use crate::raw_io::volume::VolumeLock;
use crate::raw_io::write::{open_writable, WritableDisk};
use crate::{disk, safety, ParqError, Result};

mod ntfs_boot;
mod partition_table;

/// 청크 크기(바이트). 1 MiB. 섹터 배수로 내림해 사용.
const CHUNK_BYTES: u64 = 1024 * 1024;
const PARTITION_DATA_START_BYTES: u64 = 1024 * 1024;

/// 복사 방향 (docs/v2-move-algorithm.md §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// 첫 청크부터 (non-overlap 기본, 또는 dst < src 인 겹침).
    Forward,
    /// 마지막 청크부터 (dst > src 인 겹침 — 자기잠식 방지).
    Backward,
}

/// 두 동일 길이 영역이 겹치는가.
#[must_use]
fn ranges_overlap(a_lba: u64, b_lba: u64, len: u64) -> bool {
    let a_end = a_lba.saturating_add(len);
    let b_end = b_lba.saturating_add(len);
    a_lba < b_end && b_lba < a_end
}

/// 방향 결정 (move-algorithm §2): 겹치고 dst>src → backward, 그 외 forward.
#[must_use]
fn decide_direction(src_lba: u64, dst_lba: u64, len: u64) -> Direction {
    if ranges_overlap(src_lba, dst_lba, len) && dst_lba > src_lba {
        Direction::Backward
    } else {
        Direction::Forward
    }
}

fn choose_chunk_sectors(plan: &MovePlan, base_chunk_sectors: u64) -> u64 {
    if ranges_overlap(plan.src_lba, plan.dst_lba, plan.length_sectors) {
        base_chunk_sectors.min(plan.src_lba.abs_diff(plan.dst_lba).max(1))
    } else {
        base_chunk_sectors
    }
}

fn minimum_partition_start_lba(sector_bytes: u64) -> u64 {
    PARTITION_DATA_START_BYTES.div_ceil(sector_bytes.max(1))
}

fn checkpoint_drive_letter(path: &Path) -> Result<char> {
    let value = path.to_string_lossy();
    let bytes = value.as_bytes();
    if bytes.len() < 3
        || bytes[1] != b':'
        || !bytes[0].is_ascii_alphabetic()
        || !matches!(bytes[2], b'\\' | b'/')
    {
        return Err(ParqError::ValidationFailed(format!(
            "checkpoint path must be an absolute drive path: {}",
            path.display()
        )));
    }
    Ok((bytes[0] as char).to_ascii_uppercase())
}

fn ensure_checkpoint_on_other_disk(path: &Path, target_disk_number: u32) -> Result<()> {
    let letter = checkpoint_drive_letter(path)?;
    let extent = crate::raw_io::volume::query_volume_extent(&letter.to_string())?;
    if extent.disk_number == target_disk_number {
        return Err(ParqError::ValidationFailed(
            "checkpoint volume must be on a different physical disk from the move target".into(),
        ));
    }
    Ok(())
}

/// 진행 인덱스 `i`(0-based, 방향순) → 영역 시작으로부터의 (섹터 오프셋, 이 청크의 섹터 수).
/// forward 는 앞에서부터, backward 는 뒤에서부터 물리 청크를 고른다. 나머지 청크는 마지막 물리 청크.
fn chunk_at(
    i: u64,
    chunks_total: u64,
    length_sectors: u64,
    chunk_sectors: u64,
    dir: Direction,
) -> (u64, u64) {
    let phys = match dir {
        Direction::Forward => i,
        Direction::Backward => chunks_total - 1 - i,
    };
    let offset = phys * chunk_sectors;
    let size = (length_sectors - offset).min(chunk_sectors);
    (offset, size)
}

fn physical_chunk_was_copied(
    physical_chunk: u64,
    chunks_total: u64,
    chunks_done: u64,
    direction: Direction,
) -> bool {
    match direction {
        Direction::Forward => physical_chunk < chunks_done,
        Direction::Backward => physical_chunk >= chunks_total.saturating_sub(chunks_done),
    }
}

/// 이동 계획 (read-only 로 계산).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovePlan {
    pub disk_number: u32,
    pub src_lba: u64,
    pub dst_lba: u64,
    pub length_sectors: u64,
    pub direction: Direction,
}

/// checkpoint (docs/v2-checkpoint-format.md §2). 이동 대상과 **다른 디스크**에 저장해야 한다(§3).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Checkpoint {
    log_format_version: u32,
    plan: MovePlan,
    chunk_sectors: u64,
    chunks_total: u64,
    chunks_done: u64,
    /// 이동 전 src 영역 SHA256(hex). 첫 실행에서 write 이전에 계산해 고정.
    src_sha256: Option<String>,
    /// "copying" | "verifying" | "done".
    phase: String,
}

/// 이동 결과.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveOutcome {
    pub sha256: String,
    pub chunks: u64,
    pub direction: Direction,
    pub resumed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveEvent {
    DataFlushed { chunk: u64, total: u64 },
    CheckpointPersisted { chunk: u64, total: u64 },
    BeforeTableWrite,
    AfterGptBackupFlushed,
    AfterGptPrimaryEntriesFlushed,
    AfterTableWriteFlushed,
}

/// 이동 계획 계산 (read-only). 알파 게이트 통과 필수.
#[instrument]
pub fn plan_move(
    disk_number: u32,
    src_lba: u64,
    dst_lba: u64,
    length_sectors: u64,
) -> Result<MovePlan> {
    safety::require_v2_destructive()?;
    if length_sectors == 0 {
        return Err(ParqError::ValidationFailed("이동 길이가 0".into()));
    }
    if src_lba == dst_lba {
        return Err(ParqError::ValidationFailed("src 와 dst 가 동일".into()));
    }
    Ok(MovePlan {
        disk_number,
        src_lba,
        dst_lba,
        length_sectors,
        direction: decide_direction(src_lba, dst_lba, length_sectors),
    })
}

/// 이동 실행 (checkpoint 재개 지원). `progress(chunks_done, chunks_total)` 는 매 청크 커서 기록
/// 후 호출된다(진행률/ETA + 테스트의 kill 주입점). checkpoint 는 이동 대상과 **다른 디스크**의
/// 파일이어야 한다(§3 복구 전제).
#[instrument(skip(progress), fields(disk = plan.disk_number, dir = ?plan.direction))]
pub fn execute_move<P>(
    plan: &MovePlan,
    checkpoint_path: &Path,
    mut progress: P,
) -> Result<MoveOutcome>
where
    P: FnMut(u64, u64),
{
    execute_move_with_events(plan, checkpoint_path, |event| {
        if let MoveEvent::CheckpointPersisted { chunk, total } = event {
            progress(chunk, total);
        }
    })
}

#[doc(hidden)]
pub fn execute_move_with_events<P>(
    plan: &MovePlan,
    checkpoint_path: &Path,
    mut on_event: P,
) -> Result<MoveOutcome>
where
    P: FnMut(MoveEvent),
{
    safety::require_v2_destructive()?;
    ensure_checkpoint_on_other_disk(checkpoint_path, plan.disk_number)?;

    // 알파 게이트 + 디스크 가드는 open_writable 안에서 강제된다.
    let disk = open_writable(plan.disk_number)?;
    let geo = disk.geometry();
    let sector = geo.logical_sector_bytes as u64;
    if sector == 0 {
        return Err(ParqError::Platform("논리 섹터 크기 0".into()));
    }
    let base_chunk_sectors = (CHUNK_BYTES / sector).max(1);
    let chunk_sectors = choose_chunk_sectors(plan, base_chunk_sectors);
    let chunks_total = plan.length_sectors.div_ceil(chunk_sectors);

    // 범위 검증: src/dst 어느 쪽도 디스크를 넘지 않는다.
    let max_end = plan
        .src_lba
        .max(plan.dst_lba)
        .saturating_add(plan.length_sectors);
    if max_end > geo.sector_count() {
        return Err(ParqError::ValidationFailed(format!(
            "이동 범위가 디스크를 초과: end={max_end} > sector_count={}",
            geo.sector_count()
        )));
    }

    // checkpoint 로드 또는 초기화.
    let (mut cp, resumed) = match load_checkpoint(checkpoint_path)? {
        Some(existing) => {
            // 재개: plan 이 일치해야 한다(다른 이동의 checkpoint 오용 방지).
            if existing.log_format_version != 2 || existing.plan != *plan {
                return Err(ParqError::ValidationFailed(
                    "checkpoint 의 plan 이 요청과 불일치 — 다른 이동의 로그일 수 있음".into(),
                ));
            }
            if existing.chunk_sectors != chunk_sectors
                || existing.chunks_total != chunks_total
                || existing.chunks_done > existing.chunks_total
            {
                return Err(ParqError::ValidationFailed(
                    "checkpoint 의 청크 기하가 현재 이동 계획과 불일치".into(),
                ));
            }
            warn!(target: "parq::move", chunks_done = existing.chunks_done, "checkpoint 에서 이동 재개");
            (existing, true)
        }
        None => (
            Checkpoint {
                log_format_version: 2,
                plan: plan.clone(),
                chunk_sectors,
                chunks_total,
                chunks_done: 0,
                src_sha256: None,
                phase: "copying".into(),
            },
            false,
        ),
    };

    // src SHA256 를 write 이전에 확정 (charter §3-7 라운드트립 기준값).
    // 겹침 이동에서 src 가 부분 훼손되기 전에 반드시 계산돼야 하므로 첫 write 전에.
    if cp.src_sha256.is_none() {
        if cp.chunks_done != 0 {
            return Err(ParqError::ValidationFailed(
                "checkpoint has copied chunks but no source SHA-256".into(),
            ));
        }
        let sha = hash_region(
            &disk,
            plan.src_lba,
            plan.length_sectors,
            chunk_sectors,
            sector,
        )?;
        cp.src_sha256 = Some(sha);
        write_checkpoint(checkpoint_path, &cp)?;
    }

    if resumed {
        let expected = cp
            .src_sha256
            .as_deref()
            .ok_or_else(|| ParqError::Transaction("checkpoint has no source SHA-256".into()))?;
        let reconstructed = hash_reconstructed_snapshot(&disk, &cp, sector)?;
        if reconstructed != expected {
            return Err(ParqError::ValidationFailed(format!(
                "checkpoint snapshot changed since the previous run: expected={expected}, reconstructed={reconstructed}. No additional data was written; discard this checkpoint and start from a known-good filesystem state"
            )));
        }
        info!(
            target: "parq::move",
            chunks_done = cp.chunks_done,
            sha256 = %reconstructed,
            "checkpoint snapshot validated before resume"
        );
    }

    // 청크 복사 (방향순) — checkpoint write 순서(§3): read → write(+flush) → 커서 기록(+fsync).
    let mut buf = vec![0u8; (chunk_sectors * sector) as usize];
    while cp.chunks_done < cp.chunks_total {
        let i = cp.chunks_done;
        let (offset, this_sectors) = chunk_at(
            i,
            cp.chunks_total,
            plan.length_sectors,
            chunk_sectors,
            plan.direction,
        );
        let bytes = (this_sectors * sector) as usize;
        let slice = &mut buf[..bytes];

        disk.read_sectors(plan.src_lba + offset, slice)?; // (1)
        disk.write_sectors(plan.dst_lba + offset, slice)?; // (2)+(3) write_sectors 가 flush 포함
        on_event(MoveEvent::DataFlushed {
            chunk: i + 1,
            total: cp.chunks_total,
        });

        cp.chunks_done = i + 1; // (4)
        write_checkpoint(checkpoint_path, &cp)?; // fsync
        on_event(MoveEvent::CheckpointPersisted {
            chunk: cp.chunks_done,
            total: cp.chunks_total,
        });
    }

    // 라운드트립 검증 (§3-7): dst 영역 SHA256 == 이동 전 src SHA256.
    cp.phase = "verifying".into();
    write_checkpoint(checkpoint_path, &cp)?;
    let src_sha = cp
        .src_sha256
        .clone()
        .ok_or_else(|| ParqError::Transaction("checkpoint 에 src_sha256 없음".into()))?;
    let dst_sha = hash_region(
        &disk,
        plan.dst_lba,
        plan.length_sectors,
        chunk_sectors,
        sector,
    )?;
    if dst_sha != src_sha {
        return Err(ParqError::ValidationFailed(format!(
            "destination SHA-256 mismatch: expected={src_sha}, actual={dst_sha}. \
             The partition table was not updated. For an overlapping move, the old source range \
             is not a complete rollback copy"
        )));
    }

    cp.phase = "verified".into();
    write_checkpoint(checkpoint_path, &cp)?;
    info!(target: "parq::move", sha256 = %dst_sha, chunks = cp.chunks_total, "이동 + 라운드트립 검증 완료");

    Ok(MoveOutcome {
        sha256: dst_sha,
        chunks: cp.chunks_total,
        direction: plan.direction,
        resumed,
    })
}

/// 완결된 파티션 이동 결과.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MovePartitionOutcome {
    pub data: MoveOutcome,
    pub partition_id: String,
    pub old_start_lba: u64,
    pub new_start_lba: u64,
    pub length_sectors: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveMode {
    Standard,
    OfflineSystem,
}

/// **완결된 파티션 이동**: 데이터 이동 + 인접 무변경 검증 + 파티션 테이블 갱신.
///
/// 길이는 파티션 테이블에서 읽어온다(호출자가 지정하지 않는다). 순서(charter 안전 모델):
/// 1. 알파 게이트 + 디스크 가드(`open_writable`) + 파티션 가드(부팅/시스템/마운트 거부).
/// 2. dst 가 다른 파티션과 겹치지 않는(free) 영역인지 검증.
/// 3. **인접 파티션 SHA256 스냅샷**(charter §3-6).
/// 4. 데이터 이동(checkpoint + 라운드트립).
/// 5. 인접 파티션 무변경 재확인 — 변경됐으면 테이블 미갱신(원본 src 보존).
/// 6. **모두 통과한 뒤에만** 파티션 테이블의 시작 LBA 갱신.
///
/// MBR primary 및 GPT primary/backup 파티션 테이블을 지원한다.
#[instrument(skip(checkpoint_path))]
pub fn move_partition(
    disk_number: u32,
    src_start_lba: u64,
    new_start_lba: u64,
    checkpoint_path: &Path,
) -> Result<MovePartitionOutcome> {
    move_partition_with_events_mode(
        disk_number,
        src_start_lba,
        new_start_lba,
        checkpoint_path,
        MoveMode::Standard,
        |_| {},
    )
}

pub fn move_partition_offline_system<P>(
    disk_number: u32,
    src_start_lba: u64,
    new_start_lba: u64,
    source_drive_letter: &str,
    checkpoint_path: &Path,
    on_event: P,
) -> Result<MovePartitionOutcome>
where
    P: FnMut(MoveEvent),
{
    let _volume_lock = VolumeLock::lock_and_dismount(source_drive_letter)?;
    let outcome = move_partition_with_events_mode(
        disk_number,
        src_start_lba,
        new_start_lba,
        checkpoint_path,
        MoveMode::OfflineSystem,
        on_event,
    )?;
    let disk = open_writable(disk_number)?;
    ntfs_boot::update_hidden_sectors(&disk, src_start_lba, new_start_lba, outcome.length_sectors)?;
    Ok(outcome)
}

pub fn patch_ntfs_boot_metadata_offline(
    disk_number: u32,
    old_start_lba: u64,
    new_start_lba: u64,
    length_sectors: u64,
    source_drive_letter: &str,
) -> Result<()> {
    safety::require_offline_system_move()?;
    let _volume_lock = VolumeLock::lock_and_dismount(source_drive_letter)?;
    let disk = open_writable(disk_number)?;
    ntfs_boot::update_hidden_sectors(&disk, old_start_lba, new_start_lba, length_sectors)
}

#[doc(hidden)]
pub fn move_partition_with_events<P>(
    disk_number: u32,
    src_start_lba: u64,
    new_start_lba: u64,
    checkpoint_path: &Path,
    on_event: P,
) -> Result<MovePartitionOutcome>
where
    P: FnMut(MoveEvent),
{
    move_partition_with_events_mode(
        disk_number,
        src_start_lba,
        new_start_lba,
        checkpoint_path,
        MoveMode::Standard,
        on_event,
    )
}

fn move_partition_with_events_mode<P>(
    disk_number: u32,
    src_start_lba: u64,
    new_start_lba: u64,
    checkpoint_path: &Path,
    mode: MoveMode,
    mut on_event: P,
) -> Result<MovePartitionOutcome>
where
    P: FnMut(MoveEvent),
{
    safety::require_v2_destructive()?;
    if mode == MoveMode::OfflineSystem {
        safety::require_offline_system_move()?;
    }

    let disk = open_writable(disk_number)?; // 알파 게이트 + 디스크 가드
    let sector = disk.geometry().logical_sector_bytes as u64;
    if sector == 0 {
        return Err(ParqError::Platform("논리 섹터 크기 0".into()));
    }
    let minimum_start = minimum_partition_start_lba(sector);
    if new_start_lba < minimum_start {
        return Err(ParqError::ValidationFailed(format!(
            "새 시작 LBA {new_start_lba} 는 디스크 예약 영역과 겹칩니다 — 최소 LBA {minimum_start} (1 MiB) 필요"
        )));
    }

    let disks = disk::enumerate()?;
    let layout = disks
        .iter()
        .find(|d| d.number == disk_number)
        .ok_or_else(|| ParqError::DiskNotFound(format!("disk {disk_number}")))?;

    if !matches!(
        layout.partition_style,
        disk::PartitionStyle::Mbr | disk::PartitionStyle::Gpt
    ) {
        return Err(ParqError::ValidationFailed(
            "MBR 또는 GPT 파티션 테이블만 이동할 수 있습니다".into(),
        ));
    }

    if let Some(recovered) = recover_after_table_write(
        &disk,
        layout,
        disk_number,
        src_start_lba,
        new_start_lba,
        checkpoint_path,
        sector,
    )? {
        return Ok(recovered);
    }

    let src_part = layout
        .partitions
        .iter()
        .find(|p| p.offset_bytes / sector == src_start_lba)
        .ok_or_else(|| {
            ParqError::ValidationFailed(format!("시작 LBA {src_start_lba} 인 파티션이 없습니다"))
        })?;

    let start_state = match layout.partition_style {
        disk::PartitionStyle::Mbr => {
            partition_table::read_start_state_mbr(&disk, src_start_lba, new_start_lba)?
        }
        disk::PartitionStyle::Gpt => {
            partition_table::read_start_state_gpt(&disk, src_start_lba, new_start_lba)?
        }
        _ => unreachable!("partition style validated above"),
    };
    match start_state {
        partition_table::StartState::Old => {}
        partition_table::StartState::New => {
            return Err(ParqError::ValidationFailed(
                "파티션 엔트리가 이미 새 시작 위치를 가리킵니다 — 복구 checkpoint 없이 이동할 수 없습니다"
                    .into(),
            ))
        }
        partition_table::StartState::Both | partition_table::StartState::Missing => {
            return Err(ParqError::ValidationFailed(
                "대상 파티션 엔트리를 old 위치에서 유일하게 확인할 수 없습니다"
                    .into(),
            ))
        }
    }

    match mode {
        MoveMode::Standard => safety::check_partition_destructive(layout, src_part)?,
        MoveMode::OfflineSystem => {
            safety::check_partition_offline_system_move_lockable(layout, src_part)?
        }
    }

    let length_sectors = src_part.size_bytes / sector;
    if length_sectors == 0 {
        return Err(ParqError::ValidationFailed("파티션 길이가 0".into()));
    }
    if mode == MoveMode::OfflineSystem {
        ntfs_boot::validate_move_source(&disk, src_start_lba, new_start_lba, length_sectors)?;
    }

    // dst 가 다른 파티션과 겹치지 않는지(자기 자신은 겹침 허용 — overlap 이동).
    let dst_end = new_start_lba.saturating_add(length_sectors);
    for p in &layout.partitions {
        let p_start = p.offset_bytes / sector;
        if p_start == src_start_lba {
            continue;
        }
        let p_end = p_start + p.size_bytes / sector;
        if new_start_lba < p_end && p_start < dst_end {
            return Err(ParqError::ValidationFailed(format!(
                "대상 영역이 파티션 {}({p_start}..{p_end}) 과 겹칩니다 — free 영역으로만 이동 가능",
                p.id
            )));
        }
    }

    // 인접 파티션 무변경 스냅샷 (charter §3-6).
    let adjacent_before = hash_others(&disk, layout, src_start_lba, sector)?;

    // 데이터 이동 (checkpoint + 라운드트립).
    let plan = plan_move(disk_number, src_start_lba, new_start_lba, length_sectors)?;
    let data = execute_move_with_events(&plan, checkpoint_path, &mut on_event)?;

    // 인접 무변경 재확인.
    let adjacent_after = hash_others(&disk, layout, src_start_lba, sector)?;
    if adjacent_before != adjacent_after {
        return Err(ParqError::ValidationFailed(
            "인접 파티션이 변경되었습니다! 파티션 테이블을 갱신하지 않았고 원본은 src 에 \
             보존되어 있습니다 (charter §3-6)."
                .into(),
        ));
    }

    // 데이터/무결성/인접 검증 모두 통과 → 파티션 테이블 갱신.
    let mut checkpoint = load_checkpoint(checkpoint_path)?
        .ok_or_else(|| ParqError::Transaction("이동 checkpoint 가 없습니다".into()))?;
    checkpoint.phase = match layout.partition_style {
        disk::PartitionStyle::Gpt => "table_update_backup",
        _ => "table_update",
    }
    .into();
    write_checkpoint(checkpoint_path, &checkpoint)?;
    on_event(MoveEvent::BeforeTableWrite);
    match layout.partition_style {
        disk::PartitionStyle::Mbr => partition_table::update_partition_start_mbr_with_hook(
            &disk,
            src_start_lba,
            new_start_lba,
            || on_event(MoveEvent::AfterTableWriteFlushed),
        )?,
        disk::PartitionStyle::Gpt => partition_table::update_partition_start_gpt_with_hook(
            &disk,
            src_start_lba,
            new_start_lba,
            |stage| {
                let (phase, event) = match stage {
                    partition_table::GptWriteStage::Backup => {
                        ("table_update_primary", MoveEvent::AfterGptBackupFlushed)
                    }
                    partition_table::GptWriteStage::PrimaryEntries => (
                        "table_update_primary_header",
                        MoveEvent::AfterGptPrimaryEntriesFlushed,
                    ),
                    partition_table::GptWriteStage::Complete => {
                        ("table_update_complete", MoveEvent::AfterTableWriteFlushed)
                    }
                };
                checkpoint.phase = phase.into();
                write_checkpoint(checkpoint_path, &checkpoint)?;
                on_event(event);
                Ok(())
            },
        )?,
        _ => unreachable!("partition style validated above"),
    }
    checkpoint.phase = "done".into();
    write_checkpoint(checkpoint_path, &checkpoint)?;

    info!(
        target: "parq::move",
        partition = %src_part.id,
        src_start_lba,
        new_start_lba,
        "파티션 이동 완료 (데이터 + 테이블 갱신)"
    );
    Ok(MovePartitionOutcome {
        data,
        partition_id: src_part.id.clone(),
        old_start_lba: src_start_lba,
        new_start_lba,
        length_sectors,
    })
}

fn recover_after_table_write(
    disk: &WritableDisk,
    layout: &disk::Disk,
    disk_number: u32,
    src_start_lba: u64,
    new_start_lba: u64,
    checkpoint_path: &Path,
    sector: u64,
) -> Result<Option<MovePartitionOutcome>> {
    let Some(checkpoint) = load_checkpoint(checkpoint_path)? else {
        return Ok(None);
    };
    if checkpoint.log_format_version != 2
        || checkpoint.plan.disk_number != disk_number
        || checkpoint.plan.src_lba != src_start_lba
        || checkpoint.plan.dst_lba != new_start_lba
    {
        return Ok(None);
    }
    if !checkpoint.phase.starts_with("table_update") && checkpoint.phase != "done" {
        return Ok(None);
    }
    let start_state = match layout.partition_style {
        disk::PartitionStyle::Mbr => {
            partition_table::read_start_state_mbr(disk, src_start_lba, new_start_lba)?
        }
        disk::PartitionStyle::Gpt => {
            partition_table::read_start_state_gpt_recovery(disk, src_start_lba, new_start_lba)?
        }
        _ => return Ok(None),
    };

    if layout.partition_style == disk::PartitionStyle::Gpt {
        if start_state == partition_table::StartState::Missing {
            return Err(ParqError::ValidationFailed(
                "GPT 이동 복구 중 old/new 시작 엔트리를 찾을 수 없습니다".into(),
            ));
        }
        partition_table::update_partition_start_gpt_with_hook(
            disk,
            src_start_lba,
            new_start_lba,
            |_| Ok(()),
        )?;
        return finish_table_write_recovery(
            disk,
            layout,
            disk_number,
            src_start_lba,
            new_start_lba,
            checkpoint_path,
            sector,
            checkpoint,
        )
        .map(Some);
    }

    match start_state {
        partition_table::StartState::Old => Ok(None),
        partition_table::StartState::Both | partition_table::StartState::Missing => {
            Err(ParqError::ValidationFailed(
                "MBR 이동 복구 중 old/new 시작 엔트리 상태가 모호합니다".into(),
            ))
        }
        partition_table::StartState::New => finish_table_write_recovery(
            disk,
            layout,
            disk_number,
            src_start_lba,
            new_start_lba,
            checkpoint_path,
            sector,
            checkpoint,
        )
        .map(Some),
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_table_write_recovery(
    disk: &WritableDisk,
    layout: &disk::Disk,
    disk_number: u32,
    src_start_lba: u64,
    new_start_lba: u64,
    checkpoint_path: &Path,
    sector: u64,
    mut checkpoint: Checkpoint,
) -> Result<MovePartitionOutcome> {
    let expected = checkpoint
        .src_sha256
        .clone()
        .ok_or_else(|| ParqError::Transaction("checkpoint 에 src_sha256 없음".into()))?;
    let actual = hash_region(
        disk,
        new_start_lba,
        checkpoint.plan.length_sectors,
        checkpoint.chunk_sectors,
        sector,
    )?;
    if actual != expected {
        return Err(ParqError::ValidationFailed(format!(
            "파티션 테이블 갱신 후 복구 해시 불일치: expected={expected}, actual={actual}"
        )));
    }
    disk.update_properties()?;
    checkpoint.phase = "done".into();
    write_checkpoint(checkpoint_path, &checkpoint)?;
    let partition_id = layout
        .partitions
        .iter()
        .find(|partition| {
            let start = partition.offset_bytes / sector;
            start == src_start_lba || start == new_start_lba
        })
        .map(|partition| partition.id.clone())
        .unwrap_or_else(|| format!("disk{disk_number}-moved"));
    info!(
        target: "parq::move",
        src_start_lba,
        new_start_lba,
        "파티션 테이블 write 이후 checkpoint 복구 완료"
    );
    Ok(MovePartitionOutcome {
        data: MoveOutcome {
            sha256: actual,
            chunks: checkpoint.chunks_total,
            direction: checkpoint.plan.direction,
            resumed: true,
        },
        partition_id,
        old_start_lba: src_start_lba,
        new_start_lba,
        length_sectors: checkpoint.plan.length_sectors,
    })
}

/// 이동 대상(exclude_start_lba)을 뺀 나머지 파티션들의 (id, SHA256) 목록. 정렬. 인접 무변경 검증용.
fn hash_others(
    disk: &WritableDisk,
    layout: &disk::Disk,
    exclude_start_lba: u64,
    sector: u64,
) -> Result<Vec<(String, String)>> {
    let chunk_sectors = (CHUNK_BYTES / sector).max(1);
    let mut out = Vec::new();
    for p in &layout.partitions {
        let p_start = p.offset_bytes / sector;
        if p_start == exclude_start_lba {
            continue;
        }
        let p_len = p.size_bytes / sector;
        if p_len == 0 {
            continue;
        }
        let sha = hash_region(disk, p_start, p_len, chunk_sectors, sector)?;
        out.push((p.id.clone(), sha));
    }
    out.sort();
    Ok(out)
}

/// 영역 SHA256(hex). read-only.
fn hash_region(
    disk: &WritableDisk,
    start_lba: u64,
    length_sectors: u64,
    chunk_sectors: u64,
    sector: u64,
) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; (chunk_sectors * sector) as usize];
    let mut off = 0u64;
    while off < length_sectors {
        let n = (length_sectors - off).min(chunk_sectors);
        let bytes = (n * sector) as usize;
        let slice = &mut buf[..bytes];
        disk.read_sectors(start_lba + off, slice)?;
        hasher.update(&slice[..]);
        off += n;
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn hash_reconstructed_snapshot(
    disk: &WritableDisk,
    checkpoint: &Checkpoint,
    sector: u64,
) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; (checkpoint.chunk_sectors * sector) as usize];
    for physical_chunk in 0..checkpoint.chunks_total {
        let offset = physical_chunk * checkpoint.chunk_sectors;
        let sectors = (checkpoint.plan.length_sectors - offset).min(checkpoint.chunk_sectors);
        let bytes = (sectors * sector) as usize;
        let start_lba = if physical_chunk_was_copied(
            physical_chunk,
            checkpoint.chunks_total,
            checkpoint.chunks_done,
            checkpoint.plan.direction,
        ) {
            checkpoint.plan.dst_lba + offset
        } else {
            checkpoint.plan.src_lba + offset
        };
        disk.read_sectors(start_lba, &mut buffer[..bytes])?;
        hasher.update(&buffer[..bytes]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn load_checkpoint(path: &Path) -> Result<Option<Checkpoint>> {
    match fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| ParqError::Transaction(format!("checkpoint 파싱 실패: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ParqError::Transaction(format!("checkpoint 읽기 실패: {e}"))),
    }
}

/// tmp → rename → fsync 원자적 교체 (transaction/mod.rs 패턴).
fn write_checkpoint(path: &Path, cp: &Checkpoint) -> Result<()> {
    let json = serde_json::to_vec_pretty(cp)
        .map_err(|e| ParqError::Transaction(format!("checkpoint 직렬화 실패: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
        .map_err(|e| ParqError::Transaction(format!("checkpoint tmp 생성 실패: {e}")))?;
    f.write_all(&json)
        .map_err(|e| ParqError::Transaction(format!("checkpoint 쓰기 실패: {e}")))?;
    f.sync_all()
        .map_err(|e| ParqError::Transaction(format!("checkpoint fsync 실패: {e}")))?;
    drop(f);
    fs::rename(&tmp, path)
        .map_err(|e| ParqError::Transaction(format!("checkpoint rename 실패: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_detection() {
        assert!(ranges_overlap(0, 5, 10)); // [0,10) vs [5,15)
        assert!(!ranges_overlap(0, 10, 10)); // [0,10) vs [10,20) 인접, 안 겹침
        assert!(!ranges_overlap(0, 100, 10));
        assert!(ranges_overlap(100, 95, 10));
    }

    #[test]
    fn direction_rules() {
        // non-overlap → forward
        assert_eq!(decide_direction(0, 1000, 10), Direction::Forward);
        // overlap, dst > src → backward (자기잠식 방지)
        assert_eq!(decide_direction(0, 5, 10), Direction::Backward);
        // overlap, dst < src → forward
        assert_eq!(decide_direction(100, 95, 10), Direction::Forward);
    }

    #[test]
    fn overlap_chunk_never_exceeds_move_distance() {
        let plan = MovePlan {
            disk_number: 1,
            src_lba: 10_000,
            dst_lba: 10_128,
            length_sectors: 4096,
            direction: Direction::Backward,
        };
        assert_eq!(choose_chunk_sectors(&plan, 2048), 128);
    }

    #[test]
    fn non_overlap_keeps_base_chunk_size() {
        let plan = MovePlan {
            disk_number: 1,
            src_lba: 10_000,
            dst_lba: 20_000,
            length_sectors: 4096,
            direction: Direction::Forward,
        };
        assert_eq!(choose_chunk_sectors(&plan, 2048), 2048);
    }

    #[test]
    fn partition_start_reserves_first_mib() {
        assert_eq!(minimum_partition_start_lba(512), 2048);
        assert_eq!(minimum_partition_start_lba(4096), 256);
    }

    #[test]
    fn checkpoint_path_requires_an_absolute_drive_path() {
        assert_eq!(
            checkpoint_drive_letter(Path::new("P:\\Parq\\move.json")).unwrap(),
            'P'
        );
        assert_eq!(
            checkpoint_drive_letter(Path::new("z:/Parq/move.json")).unwrap(),
            'Z'
        );
        assert!(checkpoint_drive_letter(Path::new("move.json")).is_err());
        assert!(checkpoint_drive_letter(Path::new("X:relative.json")).is_err());
    }

    #[test]
    fn chunk_at_forward_covers_all() {
        // length 10 sectors, chunk 4 → chunks: [0..4),[4..8),[8..10)
        let total = 10u64.div_ceil(4);
        assert_eq!(total, 3);
        assert_eq!(chunk_at(0, total, 10, 4, Direction::Forward), (0, 4));
        assert_eq!(chunk_at(1, total, 10, 4, Direction::Forward), (4, 4));
        assert_eq!(chunk_at(2, total, 10, 4, Direction::Forward), (8, 2)); // 나머지
    }

    #[test]
    fn chunk_at_backward_covers_all_from_end() {
        let total = 10u64.div_ceil(4); // 3
                                       // backward: i=0 → 마지막 물리 청크(offset 8, 2섹터), i=2 → 첫 청크
        assert_eq!(chunk_at(0, total, 10, 4, Direction::Backward), (8, 2));
        assert_eq!(chunk_at(1, total, 10, 4, Direction::Backward), (4, 4));
        assert_eq!(chunk_at(2, total, 10, 4, Direction::Backward), (0, 4));
    }

    #[test]
    fn chunk_at_union_is_full_region() {
        // forward/backward 모두 전체 영역을 정확히 한 번씩 덮는지(합집합=전체, 중복 없음).
        for dir in [Direction::Forward, Direction::Backward] {
            let (len, chunk) = (37u64, 8u64);
            let total = len.div_ceil(chunk);
            let mut covered = vec![false; len as usize];
            for i in 0..total {
                let (off, n) = chunk_at(i, total, len, chunk, dir);
                for s in off..off + n {
                    assert!(!covered[s as usize], "중복 커버 {s}");
                    covered[s as usize] = true;
                }
            }
            assert!(covered.iter().all(|&c| c), "미커버 섹터 존재 ({dir:?})");
        }
    }

    #[test]
    fn forward_resume_selects_copied_prefix_from_destination() {
        let selected: Vec<bool> = (0..5)
            .map(|chunk| physical_chunk_was_copied(chunk, 5, 2, Direction::Forward))
            .collect();
        assert_eq!(selected, [true, true, false, false, false]);
    }

    #[test]
    fn backward_resume_selects_copied_suffix_from_destination() {
        let selected: Vec<bool> = (0..5)
            .map(|chunk| physical_chunk_was_copied(chunk, 5, 2, Direction::Backward))
            .collect();
        assert_eq!(selected, [false, false, false, true, true]);
    }

    #[test]
    fn completed_resume_selects_every_chunk_from_destination() {
        for direction in [Direction::Forward, Direction::Backward] {
            assert!((0..5).all(|chunk| physical_chunk_was_copied(chunk, 5, 5, direction)));
        }
    }
}
