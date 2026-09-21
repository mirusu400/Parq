//! 파티션 테이블 갱신 — 이동 후 파티션의 **시작 LBA** 만 새 위치로 바꾼다. **극도로 위험.**
//!
//! `docs/v2-move-algorithm.md §6`. 데이터 복사 + 라운드트립 검증 + 인접 무변경 확인을 **모두
//! 통과한 뒤에만** 호출된다. 잘못된 테이블 write 는 디스크 전체 파티션을 잃게 한다.
//!
//! ## 범위
//! MBR primary 엔트리와 GPT primary/backup 헤더·엔트리 배열을 지원한다. GPT 는 backup 을 먼저
//! 기록한 뒤 primary 를 기록하며, 각 복사본의 엔트리 배열 CRC32와 헤더 CRC32를 다시 계산한다.
//!
//! ## 한계 (문서화)
//! CHS 필드는 갱신하지 않는다(Windows 는 LBA 사용). 확장/논리 파티션은 미지원(primary 4개만).

use crate::raw_io::write::WritableDisk;
use crate::{ParqError, Result};

const MBR_SIG_OFFSET: usize = 510;
const MBR_ENTRY_BASE: usize = 446;
const MBR_ENTRY_SIZE: usize = 16;
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_MIN_HEADER_SIZE: usize = 92;
const GPT_MIN_ENTRY_SIZE: usize = 128;
const GPT_MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartState {
    Old,
    New,
    Both,
    Missing,
}

#[derive(Debug, Clone)]
struct GptCopy {
    header_lba: u64,
    header: Vec<u8>,
    header_size: usize,
    backup_lba: u64,
    first_usable_lba: u64,
    last_usable_lba: u64,
    entry_lba: u64,
    entry_count: u32,
    entry_size: u32,
    entries_bytes: usize,
    entries: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GptWriteStage {
    Backup,
    PrimaryEntries,
    Complete,
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated slice"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("validated slice"),
    )
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn load_gpt_copy(disk: &WritableDisk, header_lba: u64) -> Result<GptCopy> {
    let sector = disk.geometry().logical_sector_bytes as usize;
    let sector_count = disk.geometry().sector_count();
    let mut header = vec![0u8; sector];
    disk.read_sectors(header_lba, &mut header)?;
    if &header[..GPT_SIGNATURE.len()] != GPT_SIGNATURE {
        return Err(ParqError::ValidationFailed(format!(
            "LBA {header_lba} 에 GPT 헤더 시그니처가 없습니다"
        )));
    }

    let header_size = read_u32(&header, 12) as usize;
    if !(GPT_MIN_HEADER_SIZE..=sector).contains(&header_size) {
        return Err(ParqError::ValidationFailed(format!(
            "GPT 헤더 크기가 잘못됐습니다: {header_size}"
        )));
    }
    if read_u64(&header, 24) != header_lba {
        return Err(ParqError::ValidationFailed(format!(
            "GPT current LBA가 실제 위치와 다릅니다: header={header_lba}, field={}",
            read_u64(&header, 24)
        )));
    }
    let stored_header_crc = read_u32(&header, 16);
    let mut crc_header = header[..header_size].to_vec();
    write_u32(&mut crc_header, 16, 0);
    let actual_header_crc = crc32(&crc_header);
    if stored_header_crc != actual_header_crc {
        return Err(ParqError::ValidationFailed(format!(
            "GPT 헤더 CRC32 불일치: stored={stored_header_crc:#010x}, actual={actual_header_crc:#010x}"
        )));
    }

    let backup_lba = read_u64(&header, 32);
    let first_usable_lba = read_u64(&header, 40);
    let last_usable_lba = read_u64(&header, 48);
    let entry_lba = read_u64(&header, 72);
    let entry_count = read_u32(&header, 80);
    let entry_size = read_u32(&header, 84);
    if entry_count == 0
        || entry_size < GPT_MIN_ENTRY_SIZE as u32
        || entry_size % 8 != 0
        || backup_lba >= sector_count
        || first_usable_lba > last_usable_lba
    {
        return Err(ParqError::ValidationFailed(
            "GPT 헤더의 엔트리 기하 또는 usable LBA 범위가 잘못됐습니다".into(),
        ));
    }
    let entries_bytes = (entry_count as usize)
        .checked_mul(entry_size as usize)
        .filter(|bytes| *bytes <= GPT_MAX_ENTRY_BYTES)
        .ok_or_else(|| {
            ParqError::ValidationFailed("GPT 엔트리 배열 크기가 지원 범위를 초과합니다".into())
        })?;
    let entry_sectors = entries_bytes.div_ceil(sector);
    if match entry_lba.checked_add(entry_sectors as u64) {
        Some(end) => end > sector_count,
        None => true,
    } {
        return Err(ParqError::ValidationFailed(
            "GPT 엔트리 배열이 디스크 범위를 벗어납니다".into(),
        ));
    }
    let mut entries = vec![0u8; entry_sectors * sector];
    disk.read_sectors(entry_lba, &mut entries)?;
    let stored_entries_crc = read_u32(&header, 88);
    let actual_entries_crc = crc32(&entries[..entries_bytes]);
    if stored_entries_crc != actual_entries_crc {
        return Err(ParqError::ValidationFailed(format!(
            "GPT 엔트리 배열 CRC32 불일치: stored={stored_entries_crc:#010x}, actual={actual_entries_crc:#010x}"
        )));
    }

    Ok(GptCopy {
        header_lba,
        header,
        header_size,
        backup_lba,
        first_usable_lba,
        last_usable_lba,
        entry_lba,
        entry_count,
        entry_size,
        entries_bytes,
        entries,
    })
}

fn load_gpt_copies(disk: &WritableDisk) -> Result<(Option<GptCopy>, Option<GptCopy>)> {
    let last_lba = disk.geometry().sector_count().saturating_sub(1);
    let primary = load_gpt_copy(disk, 1).ok();
    let backup = load_gpt_copy(disk, last_lba).ok();
    if primary.is_none() && backup.is_none() {
        return Err(ParqError::ValidationFailed(
            "primary/backup GPT 헤더와 엔트리 배열이 모두 유효하지 않습니다".into(),
        ));
    }
    Ok((primary, backup))
}

fn gpt_copies_compatible(primary: &GptCopy, backup: &GptCopy) -> bool {
    primary.header_lba == backup.backup_lba
        && backup.header_lba == primary.backup_lba
        && primary.first_usable_lba == backup.first_usable_lba
        && primary.last_usable_lba == backup.last_usable_lba
        && primary.entry_count == backup.entry_count
        && primary.entry_size == backup.entry_size
        && primary.header[56..72] == backup.header[56..72]
}

fn gpt_copy_start_state(copy: &GptCopy, old_start_lba: u64, new_start_lba: u64) -> StartState {
    let mut old_found = false;
    let mut new_found = false;
    for index in 0..copy.entry_count as usize {
        let base = index * copy.entry_size as usize;
        if copy.entries[base..base + 16].iter().all(|byte| *byte == 0) {
            continue;
        }
        let start = read_u64(&copy.entries, base + 32);
        old_found |= start == old_start_lba;
        new_found |= start == new_start_lba;
    }
    match (old_found, new_found) {
        (true, false) => StartState::Old,
        (false, true) => StartState::New,
        (true, true) => StartState::Both,
        (false, false) => StartState::Missing,
    }
}

fn combine_states(states: impl IntoIterator<Item = StartState>) -> StartState {
    let mut old_found = false;
    let mut new_found = false;
    for state in states {
        old_found |= matches!(state, StartState::Old | StartState::Both);
        new_found |= matches!(state, StartState::New | StartState::Both);
    }
    match (old_found, new_found) {
        (true, false) => StartState::Old,
        (false, true) => StartState::New,
        (true, true) => StartState::Both,
        (false, false) => StartState::Missing,
    }
}

pub(super) fn read_start_state_gpt(
    disk: &WritableDisk,
    old_start_lba: u64,
    new_start_lba: u64,
) -> Result<StartState> {
    let (primary, backup) = load_gpt_copies(disk)?;
    let (primary, backup) = match (primary, backup) {
        (Some(primary), Some(backup)) if gpt_copies_compatible(&primary, &backup) => {
            (primary, backup)
        }
        (Some(_), Some(_)) => {
            return Err(ParqError::ValidationFailed(
                "primary/backup GPT 메타데이터가 서로 일치하지 않습니다".into(),
            ))
        }
        _ => {
            return Err(ParqError::ValidationFailed(
                "GPT 이동 시작 전 primary/backup 복사본이 모두 유효해야 합니다".into(),
            ))
        }
    };
    Ok(combine_states([
        gpt_copy_start_state(&primary, old_start_lba, new_start_lba),
        gpt_copy_start_state(&backup, old_start_lba, new_start_lba),
    ]))
}

pub(super) fn read_start_state_gpt_recovery(
    disk: &WritableDisk,
    old_start_lba: u64,
    new_start_lba: u64,
) -> Result<StartState> {
    let (primary, backup) = load_gpt_copies(disk)?;
    Ok(combine_states(primary.iter().chain(backup.iter()).map(
        |copy| gpt_copy_start_state(copy, old_start_lba, new_start_lba),
    )))
}

fn build_gpt_header(
    source: &GptCopy,
    current_lba: u64,
    backup_lba: u64,
    entry_lba: u64,
    entries_crc: u32,
) -> Vec<u8> {
    let mut header = source.header.clone();
    write_u64(&mut header, 24, current_lba);
    write_u64(&mut header, 32, backup_lba);
    write_u64(&mut header, 72, entry_lba);
    write_u32(&mut header, 88, entries_crc);
    write_u32(&mut header, 16, 0);
    let header_crc = crc32(&header[..source.header_size]);
    write_u32(&mut header, 16, header_crc);
    header
}

pub(super) fn update_partition_start_gpt_with_hook<F>(
    disk: &WritableDisk,
    old_start_lba: u64,
    new_start_lba: u64,
    after_write: F,
) -> Result<()>
where
    F: FnMut(GptWriteStage) -> Result<()>,
{
    let mut after_write = after_write;
    let sector = disk.geometry().logical_sector_bytes as usize;
    let last_lba = disk.geometry().sector_count().saturating_sub(1);
    let (primary, backup) = load_gpt_copies(disk)?;
    let source = primary
        .as_ref()
        .filter(|copy| {
            !matches!(
                gpt_copy_start_state(copy, old_start_lba, new_start_lba),
                StartState::Missing | StartState::Both
            )
        })
        .or_else(|| {
            backup.as_ref().filter(|copy| {
                !matches!(
                    gpt_copy_start_state(copy, old_start_lba, new_start_lba),
                    StartState::Missing | StartState::Both
                )
            })
        })
        .ok_or_else(|| {
            ParqError::ValidationFailed(
                "유효한 GPT 복사본에서 이동 대상 엔트리를 유일하게 찾을 수 없습니다".into(),
            )
        })?;

    let entry_sectors = source.entries.len() / sector;
    let primary_entry_lba = primary.as_ref().map_or(2, |copy| copy.entry_lba);
    let backup_entry_lba = backup.as_ref().map_or_else(
        || {
            last_lba.checked_sub(entry_sectors as u64).ok_or_else(|| {
                ParqError::ValidationFailed("GPT backup 엔트리 위치 계산 실패".into())
            })
        },
        |copy| Ok(copy.entry_lba),
    )?;
    if source.backup_lba != 1 && source.header_lba != 1 {
        return Err(ParqError::ValidationFailed(
            "표준 Windows GPT(primary header LBA 1)만 이동할 수 있습니다".into(),
        ));
    }
    if primary_entry_lba + entry_sectors as u64 > source.first_usable_lba
        || backup_entry_lba <= source.last_usable_lba
    {
        return Err(ParqError::ValidationFailed(
            "GPT 엔트리 배열과 usable LBA 범위가 겹칩니다".into(),
        ));
    }

    let mut entries = source.entries.clone();
    let mut target = None;
    for index in 0..source.entry_count as usize {
        let base = index * source.entry_size as usize;
        if entries[base..base + 16].iter().all(|byte| *byte == 0) {
            continue;
        }
        let start = read_u64(&entries, base + 32);
        if (start == old_start_lba || start == new_start_lba) && target.replace(base).is_some() {
            return Err(ParqError::ValidationFailed(
                "GPT 이동 대상 엔트리가 둘 이상입니다".into(),
            ));
        }
    }
    let base = target.ok_or_else(|| {
        ParqError::ValidationFailed("GPT 이동 대상 엔트리를 찾을 수 없습니다".into())
    })?;
    let current_start = read_u64(&entries, base + 32);
    let current_end = read_u64(&entries, base + 40);
    if current_end < current_start {
        return Err(ParqError::ValidationFailed(
            "GPT 엔트리의 끝 LBA가 시작 LBA보다 작습니다".into(),
        ));
    }
    let length = current_end - current_start + 1;
    let new_end_lba = new_start_lba
        .checked_add(length - 1)
        .ok_or_else(|| ParqError::ValidationFailed("GPT 새 끝 LBA overflow".into()))?;
    if new_start_lba < source.first_usable_lba || new_end_lba > source.last_usable_lba {
        return Err(ParqError::ValidationFailed(format!(
            "GPT 새 파티션 범위 {new_start_lba}..={new_end_lba} 가 usable 범위를 벗어납니다"
        )));
    }
    write_u64(&mut entries, base + 32, new_start_lba);
    write_u64(&mut entries, base + 40, new_end_lba);
    let entries_crc = crc32(&entries[..source.entries_bytes]);
    let primary_header = build_gpt_header(source, 1, last_lba, primary_entry_lba, entries_crc);
    let backup_header = build_gpt_header(source, last_lba, 1, backup_entry_lba, entries_crc);

    disk.write_sectors(backup_entry_lba, &entries)?;
    disk.write_sectors(last_lba, &backup_header)?;
    after_write(GptWriteStage::Backup)?;
    disk.write_sectors(primary_entry_lba, &entries)?;
    after_write(GptWriteStage::PrimaryEntries)?;
    disk.write_sectors(1, &primary_header)?;
    disk.flush()?;
    after_write(GptWriteStage::Complete)?;

    if read_start_state_gpt(disk, old_start_lba, new_start_lba)? != StartState::New {
        return Err(ParqError::ValidationFailed(
            "GPT primary/backup 갱신 후 새 시작 LBA 검증에 실패했습니다".into(),
        ));
    }
    disk.update_properties()?;
    Ok(())
}

/// MBR primary 파티션의 시작 LBA 를 `old_start_lba` → `new_start_lba` 로 갱신한다.
///
/// LBA0 을 읽어 시그니처를 확인하고, 시작 LBA 가 `old` 인 엔트리를 찾아 `new` 로 바꾼 뒤 LBA0 을
/// 다시 쓴다(단일 섹터 원자적 갱신 + flush). 파티션 크기 필드는 건드리지 않는다(이동은 시작만 이동).
pub(super) fn update_partition_start_mbr_with_hook<F>(
    disk: &WritableDisk,
    old_start_lba: u64,
    new_start_lba: u64,
    after_write: F,
) -> Result<()>
where
    F: FnOnce(),
{
    let sector = disk.geometry().logical_sector_bytes as usize;
    let mut mbr = vec![0u8; sector];
    disk.read_sectors(0, &mut mbr)?;

    if mbr[MBR_SIG_OFFSET] != 0x55 || mbr[MBR_SIG_OFFSET + 1] != 0xAA {
        return Err(ParqError::ValidationFailed(
            "MBR 부트 시그니처(0x55AA)가 없습니다 — MBR 디스크가 아니거나 손상".into(),
        ));
    }

    let old32: u32 = old_start_lba
        .try_into()
        .map_err(|_| ParqError::ValidationFailed("시작 LBA 가 MBR(u32) 범위를 초과".into()))?;
    let new32: u32 = new_start_lba
        .try_into()
        .map_err(|_| ParqError::ValidationFailed("새 시작 LBA 가 MBR(u32) 범위를 초과".into()))?;

    let mut target: Option<usize> = None;
    for i in 0..4 {
        let base = MBR_ENTRY_BASE + i * MBR_ENTRY_SIZE;
        let ptype = mbr[base + 4];
        if ptype == 0 {
            continue; // 빈 엔트리
        }
        let start =
            u32::from_le_bytes([mbr[base + 8], mbr[base + 9], mbr[base + 10], mbr[base + 11]]);
        if start == old32 {
            target = Some(base);
            break;
        }
    }
    let base = target.ok_or_else(|| {
        ParqError::ValidationFailed(format!(
            "시작 LBA {old_start_lba} 인 MBR primary 파티션 엔트리를 찾을 수 없음"
        ))
    })?;

    // 시작 LBA(bytes 8..12) 만 교체. CHS(1..4, 5..8) 는 유지(Windows 는 LBA 사용).
    mbr[base + 8..base + 12].copy_from_slice(&new32.to_le_bytes());

    disk.write_sectors(0, &mbr)?;
    disk.flush()?;
    after_write();
    disk.update_properties()?;
    Ok(())
}

pub(super) fn read_start_state_mbr(
    disk: &WritableDisk,
    old_start_lba: u64,
    new_start_lba: u64,
) -> Result<StartState> {
    let sector = disk.geometry().logical_sector_bytes as usize;
    let mut mbr = vec![0u8; sector];
    disk.read_sectors(0, &mut mbr)?;
    if mbr[MBR_SIG_OFFSET] != 0x55 || mbr[MBR_SIG_OFFSET + 1] != 0xAA {
        return Err(ParqError::ValidationFailed(
            "MBR 부트 시그니처(0x55AA)가 없습니다 — MBR 디스크가 아니거나 손상".into(),
        ));
    }
    let old32: u32 = old_start_lba
        .try_into()
        .map_err(|_| ParqError::ValidationFailed("시작 LBA 가 MBR(u32) 범위를 초과".into()))?;
    let new32: u32 = new_start_lba
        .try_into()
        .map_err(|_| ParqError::ValidationFailed("새 시작 LBA 가 MBR(u32) 범위를 초과".into()))?;
    let mut old_found = false;
    let mut new_found = false;
    for i in 0..4 {
        let base = MBR_ENTRY_BASE + i * MBR_ENTRY_SIZE;
        if mbr[base + 4] == 0 {
            continue;
        }
        let start =
            u32::from_le_bytes([mbr[base + 8], mbr[base + 9], mbr[base + 10], mbr[base + 11]]);
        old_found |= start == old32;
        new_found |= start == new32;
    }
    Ok(match (old_found, new_found) {
        (true, false) => StartState::Old,
        (false, true) => StartState::New,
        (true, true) => StartState::Both,
        (false, false) => StartState::Missing,
    })
}

#[cfg(test)]
mod tests {
    // 순수 파싱/갱신 로직을 디스크 없이 검증하기 위해 로직을 버퍼 단위로 재현.
    const BASE: usize = 446;

    fn entry_start(mbr: &[u8], i: usize) -> u32 {
        let b = BASE + i * 16;
        u32::from_le_bytes([mbr[b + 8], mbr[b + 9], mbr[b + 10], mbr[b + 11]])
    }

    fn make_mbr() -> Vec<u8> {
        let mut m = vec![0u8; 512];
        m[510] = 0x55;
        m[511] = 0xAA;
        // 엔트리0: type=0x07(NTFS), start=2048, size=1000
        let b = BASE;
        m[b + 4] = 0x07;
        m[b + 8..b + 12].copy_from_slice(&2048u32.to_le_bytes());
        m[b + 12..b + 16].copy_from_slice(&1000u32.to_le_bytes());
        m
    }

    #[test]
    fn finds_and_updates_start_only() {
        let mut m = make_mbr();
        // update start 2048 -> 5000, size 불변
        let b = BASE;
        assert_eq!(entry_start(&m, 0), 2048);
        m[b + 8..b + 12].copy_from_slice(&5000u32.to_le_bytes());
        assert_eq!(entry_start(&m, 0), 5000);
        let size = u32::from_le_bytes([m[b + 12], m[b + 13], m[b + 14], m[b + 15]]);
        assert_eq!(size, 1000, "크기는 불변이어야 함");
    }

    #[test]
    fn signature_present() {
        let m = make_mbr();
        assert_eq!(m[510], 0x55);
        assert_eq!(m[511], 0xAA);
    }

    #[test]
    fn crc32_matches_standard_vector() {
        assert_eq!(super::crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn gpt_entry_state_distinguishes_old_and_new() {
        let mut copy = super::GptCopy {
            header_lba: 1,
            header: vec![0; 512],
            header_size: 92,
            backup_lba: 999,
            first_usable_lba: 34,
            last_usable_lba: 966,
            entry_lba: 2,
            entry_count: 2,
            entry_size: 128,
            entries_bytes: 256,
            entries: vec![0; 512],
        };
        copy.entries[0] = 1;
        super::write_u64(&mut copy.entries, 32, 2048);
        super::write_u64(&mut copy.entries, 40, 4095);
        assert_eq!(
            super::gpt_copy_start_state(&copy, 2048, 8192),
            super::StartState::Old
        );
        super::write_u64(&mut copy.entries, 32, 8192);
        assert_eq!(
            super::gpt_copy_start_state(&copy, 2048, 8192),
            super::StartState::New
        );
    }
}
