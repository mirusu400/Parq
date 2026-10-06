use crate::raw_io::volume::VolumeLock;
use crate::raw_io::write::WritableDisk;
use crate::{ParqError, Result};

const OEM_ID: &[u8; 8] = b"NTFS    ";
const BYTES_PER_SECTOR_OFFSET: usize = 11;
const HIDDEN_SECTORS_OFFSET: usize = 28;
const TOTAL_SECTORS_OFFSET: usize = 40;
const VOLUME_SERIAL_OFFSET: usize = 72;
const BOOT_SIGNATURE_OFFSET: usize = 510;

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("validated slice"),
    )
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

fn validate_boot_sector(bytes: &[u8], sector_bytes: u64, length_sectors: u64) -> Result<()> {
    if bytes.len() < 512
        || &bytes[3..11] != OEM_ID
        || bytes[BOOT_SIGNATURE_OFFSET] != 0x55
        || bytes[BOOT_SIGNATURE_OFFSET + 1] != 0xAA
    {
        return Err(ParqError::ValidationFailed(
            "NTFS boot sector 서명/OEM ID가 올바르지 않습니다".into(),
        ));
    }
    if u64::from(read_u16(bytes, BYTES_PER_SECTOR_OFFSET)) != sector_bytes {
        return Err(ParqError::ValidationFailed(format!(
            "NTFS bytes-per-sector가 디스크 논리 섹터와 다릅니다: ntfs={}, disk={sector_bytes}",
            read_u16(bytes, BYTES_PER_SECTOR_OFFSET)
        )));
    }
    let total_sectors = read_u64(bytes, TOTAL_SECTORS_OFFSET);
    if total_sectors == 0 || total_sectors > length_sectors {
        return Err(ParqError::ValidationFailed(format!(
            "NTFS total sectors가 파티션 범위를 벗어납니다: ntfs={total_sectors}, partition={length_sectors}"
        )));
    }
    Ok(())
}

fn volume_serial(bytes: &[u8]) -> &[u8] {
    &bytes[VOLUME_SERIAL_OFFSET..VOLUME_SERIAL_OFFSET + 8]
}

pub(super) fn validate_move_source(
    disk: &WritableDisk,
    old_start_lba: u64,
    new_start_lba: u64,
    length_sectors: u64,
) -> Result<()> {
    if length_sectors < 2 {
        return Err(ParqError::ValidationFailed(
            "NTFS 파티션 길이가 너무 작습니다".into(),
        ));
    }
    let sector_bytes = disk.geometry().logical_sector_bytes as u64;
    let primary_candidates = [old_start_lba, new_start_lba];
    let backup_candidates = [
        old_start_lba
            .checked_add(length_sectors - 1)
            .ok_or_else(|| ParqError::ValidationFailed("NTFS backup LBA overflow".into()))?,
        new_start_lba
            .checked_add(length_sectors - 1)
            .ok_or_else(|| ParqError::ValidationFailed("NTFS backup LBA overflow".into()))?,
    ];

    let mut valid_primaries = Vec::new();
    for candidate_lba in primary_candidates {
        let mut bytes = vec![0u8; sector_bytes as usize];
        disk.read_sectors(candidate_lba, &mut bytes)?;
        if validate_boot_sector(&bytes, sector_bytes, length_sectors).is_ok() {
            valid_primaries.push(bytes);
        }
    }
    let mut valid_backups = Vec::new();
    for candidate_lba in backup_candidates {
        let mut bytes = vec![0u8; sector_bytes as usize];
        disk.read_sectors(candidate_lba, &mut bytes)?;
        if validate_boot_sector(&bytes, sector_bytes, length_sectors).is_ok() {
            valid_backups.push(bytes);
        }
    }

    if !valid_primaries.iter().any(|primary| {
        valid_backups
            .iter()
            .any(|backup| volume_serial(primary) == volume_serial(backup))
    }) {
        return Err(ParqError::ValidationFailed(
            "이동 원본/대상에서 일치하는 NTFS primary/backup boot sector를 찾지 못했습니다".into(),
        ));
    }
    Ok(())
}

fn patch_hidden_sectors(bytes: &mut [u8], old_start_lba: u64, new_start_lba: u64) -> Result<()> {
    let old_start: u32 = old_start_lba.try_into().map_err(|_| {
        ParqError::ValidationFailed("NTFS hidden sectors의 u32 범위를 초과했습니다".into())
    })?;
    let new_start: u32 = new_start_lba.try_into().map_err(|_| {
        ParqError::ValidationFailed("NTFS hidden sectors의 u32 범위를 초과했습니다".into())
    })?;
    let current = read_u32(bytes, HIDDEN_SECTORS_OFFSET);
    if current != old_start && current != new_start {
        return Err(ParqError::ValidationFailed(format!(
            "NTFS hidden sectors가 이동 계획과 다릅니다: current={current}, old={old_start}, new={new_start}"
        )));
    }
    bytes[HIDDEN_SECTORS_OFFSET..HIDDEN_SECTORS_OFFSET + 4]
        .copy_from_slice(&new_start.to_le_bytes());
    Ok(())
}

pub(super) fn update_hidden_sectors_on_locked_volume(
    volume: &VolumeLock,
    old_start_lba: u64,
    new_start_lba: u64,
    length_sectors: u64,
    sector_bytes: u64,
) -> Result<()> {
    if length_sectors < 2 || sector_bytes < 512 {
        return Err(ParqError::ValidationFailed(
            "NTFS volume geometry is invalid for boot metadata patching".into(),
        ));
    }
    let sector_len: usize = sector_bytes
        .try_into()
        .map_err(|_| ParqError::ValidationFailed("NTFS sector size exceeds usize".into()))?;
    let backup_offset = (length_sectors - 1)
        .checked_mul(sector_bytes)
        .ok_or_else(|| ParqError::ValidationFailed("NTFS backup byte offset overflow".into()))?;
    let mut primary = vec![0u8; sector_len];
    let mut backup = vec![0u8; sector_len];
    volume.read_exact_at(0, &mut primary)?;
    volume.read_exact_at(backup_offset, &mut backup)?;
    validate_boot_sector(&primary, sector_bytes, length_sectors)?;
    validate_boot_sector(&backup, sector_bytes, length_sectors)?;
    if volume_serial(&primary) != volume_serial(&backup) {
        return Err(ParqError::ValidationFailed(
            "NTFS primary/backup boot sector volume serial mismatch".into(),
        ));
    }
    patch_hidden_sectors(&mut primary, old_start_lba, new_start_lba)?;
    patch_hidden_sectors(&mut backup, old_start_lba, new_start_lba)?;
    volume.write_all_at(0, &primary)?;
    volume.write_all_at(backup_offset, &backup)?;
    volume.flush()?;

    let mut verify_primary = vec![0u8; sector_len];
    let mut verify_backup = vec![0u8; sector_len];
    volume.read_exact_at(0, &mut verify_primary)?;
    volume.read_exact_at(backup_offset, &mut verify_backup)?;
    let expected: u32 = new_start_lba
        .try_into()
        .map_err(|_| ParqError::ValidationFailed("NTFS hidden sectors exceeds u32".into()))?;
    if verify_primary != primary
        || verify_backup != backup
        || read_u32(&verify_primary, HIDDEN_SECTORS_OFFSET) != expected
        || read_u32(&verify_backup, HIDDEN_SECTORS_OFFSET) != expected
    {
        return Err(ParqError::ValidationFailed(
            "NTFS volume-handle boot metadata verification failed".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot_sector(hidden: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 512];
        bytes[3..11].copy_from_slice(OEM_ID);
        bytes[BYTES_PER_SECTOR_OFFSET..BYTES_PER_SECTOR_OFFSET + 2]
            .copy_from_slice(&512u16.to_le_bytes());
        bytes[HIDDEN_SECTORS_OFFSET..HIDDEN_SECTORS_OFFSET + 4]
            .copy_from_slice(&hidden.to_le_bytes());
        bytes[TOTAL_SECTORS_OFFSET..TOTAL_SECTORS_OFFSET + 8]
            .copy_from_slice(&1000u64.to_le_bytes());
        bytes[VOLUME_SERIAL_OFFSET..VOLUME_SERIAL_OFFSET + 8]
            .copy_from_slice(&0x1234_5678_9ABC_DEF0u64.to_le_bytes());
        bytes[BOOT_SIGNATURE_OFFSET] = 0x55;
        bytes[BOOT_SIGNATURE_OFFSET + 1] = 0xAA;
        bytes
    }

    #[test]
    fn patches_old_hidden_sectors_and_is_idempotent() {
        let mut bytes = boot_sector(2048);
        validate_boot_sector(&bytes, 512, 1000).unwrap();
        patch_hidden_sectors(&mut bytes, 2048, 4096).unwrap();
        assert_eq!(read_u32(&bytes, HIDDEN_SECTORS_OFFSET), 4096);
        patch_hidden_sectors(&mut bytes, 2048, 4096).unwrap();
        assert_eq!(read_u32(&bytes, HIDDEN_SECTORS_OFFSET), 4096);
    }

    #[test]
    fn rejects_unrelated_hidden_sectors() {
        let mut bytes = boot_sector(1234);
        assert!(patch_hidden_sectors(&mut bytes, 2048, 4096).is_err());
    }
}
