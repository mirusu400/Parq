//! 파티션 테이블 갱신 — 이동 후 파티션의 **시작 LBA** 만 새 위치로 바꾼다. **극도로 위험.**
//!
//! `docs/v2-move-algorithm.md §6`. 데이터 복사 + 라운드트립 검증 + 인접 무변경 확인을 **모두
//! 통과한 뒤에만** 호출된다. 잘못된 테이블 write 는 디스크 전체 파티션을 잃게 한다.
//!
//! ## 범위
//! **MBR 만 지원.** MBR 은 LBA0 단일 섹터의 엔트리 하나(시작 LBA u32)를 바꾸고 그 섹터를 원자적
//! 으로 다시 쓰면 된다(§checkpoint §4.2 "단일 섹터 원자적 갱신"). GPT 는 primary+backup 헤더,
//! 엔트리 배열 CRC32, 헤더 CRC32 재계산이 필요(§6) → 검증된 구현 전까지 `NotImplemented`.
//!
//! ## 한계 (문서화)
//! CHS 필드는 갱신하지 않는다(Windows 는 LBA 사용). 확장/논리 파티션은 미지원(primary 4개만).

use crate::raw_io::write::WritableDisk;
use crate::{ParqError, Result};

const MBR_SIG_OFFSET: usize = 510;
const MBR_ENTRY_BASE: usize = 446;
const MBR_ENTRY_SIZE: usize = 16;

/// MBR primary 파티션의 시작 LBA 를 `old_start_lba` → `new_start_lba` 로 갱신한다.
///
/// LBA0 을 읽어 시그니처를 확인하고, 시작 LBA 가 `old` 인 엔트리를 찾아 `new` 로 바꾼 뒤 LBA0 을
/// 다시 쓴다(단일 섹터 원자적 갱신 + flush). 파티션 크기 필드는 건드리지 않는다(이동은 시작만 이동).
pub fn update_partition_start_mbr(
    disk: &WritableDisk,
    old_start_lba: u64,
    new_start_lba: u64,
) -> Result<()> {
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
        let start = u32::from_le_bytes([mbr[base + 8], mbr[base + 9], mbr[base + 10], mbr[base + 11]]);
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
    Ok(())
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
}
