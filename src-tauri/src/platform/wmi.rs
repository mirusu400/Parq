// Windows 플랫폼 어댑터.
//
// V1 에서는 직접 IOCTL 을 호출하지 않고 WMI 의 ROOT\Microsoft\Windows\Storage 네임스페이스
// (MSFT_Disk / MSFT_Partition / MSFT_Volume) 를 사용해 read-only 열거만 한다.
// 쓰기 경로는 추후 platform 모듈의 별도 파일에서 diskpart / PowerShell 래퍼로 추가한다.

use std::collections::HashMap;

use serde::Deserialize;
use tracing::{debug, info, instrument, warn};
use wmi::{COMLibrary, WMIConnection};

use crate::disk::{BitLockerStatus, BusType, Disk, FileSystemKind, Partition, PartitionStyle};
use crate::{ParqError, Result};

const STORAGE_NS: &str = "ROOT\\Microsoft\\Windows\\Storage";
const EFI_GPT_TYPE: &str = "{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}";
/// Microsoft Recovery Partition GPT type. Microsoft 규약상 항상 NTFS.
/// 이런 파티션은 보통 드라이브 문자가 없어 MSFT_Volume 과 매칭이 안 되는데,
/// GptType 으로 NTFS 임을 단정할 수 있다.
const RECOVERY_GPT_TYPE: &str = "{de94bba4-06d1-4d40-a16a-bfd50179d6ac}";

/// MSFT_Disk WMI 클래스의 필요한 속성만 역직렬화.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct MsftDisk {
    number: u32,
    model: Option<String>,
    serial_number: Option<String>,
    size: u64,
    bus_type: u16,
    partition_style: u16,
    is_system: bool,
    is_boot: bool,
    is_read_only: bool,
    unique_id: Option<String>,
}

/// MSFT_Partition WMI 클래스의 필요한 속성만 역직렬화.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct MsftPartition {
    disk_number: u32,
    partition_number: u32,
    /// WMI Char16 — 드라이브 문자가 없으면 "\0", 있으면 "C" 등 단일 문자 String 으로 도착.
    drive_letter: Option<String>,
    offset: u64,
    size: u64,
    is_boot: bool,
    is_system: bool,
    is_hidden: bool,
    #[serde(default)]
    gpt_type: Option<String>,
    #[serde(default)]
    access_paths: Option<Vec<String>>,
}

/// MSFT_Volume — 파티션의 파일시스템 / 라벨 / 사용 중 여부 출처.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct MsftVolume {
    drive_letter: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    file_system: Option<String>,
    #[serde(default)]
    file_system_label: Option<String>,
}

/// BitLocker 상태 출처. DeviceID 는 MSFT_Volume.Path 와 같은 볼륨 GUID 경로다.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct Win32EncryptableVolume {
    device_id: String,
    drive_letter: Option<String>,
    protection_status: Option<u32>,
    conversion_status: Option<u32>,
}

/// 시스템에 연결된 모든 디스크와 파티션을 열거한다. 블로킹 호출.
///
/// 호출자(Tauri command)는 `spawn_blocking` 으로 감싸 비동기 컨텍스트를 막지 않도록 한다.
#[instrument]
pub fn enumerate_disks() -> Result<Vec<Disk>> {
    let com =
        COMLibrary::new().map_err(|e| ParqError::Platform(format!("COM 초기화 실패: {e}")))?;
    let conn = WMIConnection::with_namespace_path(STORAGE_NS, com)
        .map_err(|e| ParqError::Platform(format!("Storage WMI 네임스페이스 연결 실패: {e}")))?;

    let raw_disks: Vec<MsftDisk> = conn
        .raw_query("SELECT * FROM MSFT_Disk")
        .map_err(|e| ParqError::Platform(format!("MSFT_Disk 쿼리 실패: {e}")))?;
    let raw_partitions: Vec<MsftPartition> = conn
        .raw_query("SELECT * FROM MSFT_Partition")
        .map_err(|e| ParqError::Platform(format!("MSFT_Partition 쿼리 실패: {e}")))?;
    let raw_volumes: Vec<MsftVolume> = conn
        .raw_query("SELECT * FROM MSFT_Volume")
        .map_err(|e| ParqError::Platform(format!("MSFT_Volume 쿼리 실패: {e}")))?;
    let (bitlocker_query_ok, bitlocker_by_mount) = query_bitlocker_statuses();

    info!(
        target: "parq::platform",
        disk_count = raw_disks.len(),
        partition_count = raw_partitions.len(),
        volume_count = raw_volumes.len(),
        "WMI 열거 완료"
    );

    let volumes_by_letter: HashMap<char, &MsftVolume> = raw_volumes
        .iter()
        .filter_map(|v| char_from_wmi_letter(v.drive_letter.as_deref()).map(|c| (c, v)))
        .collect();
    let volumes_by_path: HashMap<&str, &MsftVolume> = raw_volumes
        .iter()
        .filter_map(|v| v.path.as_deref().map(|p| (p, v)))
        .collect();

    let mut partitions_by_disk: HashMap<u32, Vec<&MsftPartition>> = HashMap::new();
    for p in &raw_partitions {
        partitions_by_disk.entry(p.disk_number).or_default().push(p);
    }

    let mut disks: Vec<Disk> = raw_disks
        .iter()
        .map(|d| {
            let parts = partitions_by_disk
                .get(&d.number)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            map_disk(
                d,
                parts,
                &volumes_by_letter,
                &volumes_by_path,
                bitlocker_query_ok,
                &bitlocker_by_mount,
            )
        })
        .collect();

    // 안정적인 순서로 정렬 (디스크 번호 오름차순).
    disks.sort_by_key(|d| d.number);
    Ok(disks)
}

fn map_disk(
    raw: &MsftDisk,
    raw_parts: &[&MsftPartition],
    vols_by_letter: &HashMap<char, &MsftVolume>,
    vols_by_path: &HashMap<&str, &MsftVolume>,
    bitlocker_query_ok: bool,
    bitlocker_by_mount: &HashMap<String, BitLockerStatus>,
) -> Disk {
    let serial = raw
        .serial_number
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let model = raw
        .model
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown".to_string());
    // ID 는 항상 Disk Number 를 포함시켜 세션 내 유일성을 보장한다.
    // MS 문서상 MSFT_Disk.UniqueId 가 stable identifier 이지만 VMware 가상 NVMe 같은
    // 환경에선 모든 디스크가 같은 UniqueId/SerialNumber 를 보고하므로 의존할 수 없다.
    // V1 은 세션 간 ID persistence 가 없으므로 Number 기반으로 충분.
    let stable_hint = raw
        .unique_id
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| serial.clone())
        .unwrap_or_else(|| "disk".to_string());
    let id = format!("{stable_hint}#{}", raw.number);

    let mut partitions: Vec<Partition> = raw_parts
        .iter()
        .map(|p| {
            map_partition(
                p,
                vols_by_letter,
                vols_by_path,
                bitlocker_query_ok,
                bitlocker_by_mount,
            )
        })
        .collect();
    partitions.sort_by_key(|p| p.offset_bytes);

    Disk {
        id,
        number: raw.number,
        model,
        serial,
        size_bytes: raw.size,
        bus_type: bus_type_from_wmi(raw.bus_type),
        partition_style: partition_style_from_wmi(raw.partition_style),
        // 외장 클래스(USB/SD/MMC/IEEE1394)는 제거 가능으로 본다. MSFT_Disk 는 IsRemovable 을
        // 직접 노출하지 않으므로 BusType 으로부터 도출. V2 에서 MSFT_PhysicalDisk 로 보강 검토.
        is_removable: bus_type_from_wmi(raw.bus_type).is_removable_class(),
        is_system: raw.is_system || raw.is_boot,
        is_read_only: raw.is_read_only,
        partitions,
        // disk::enumerate 가 후처리로 safety 가드를 돌려 채운다. 기본 false 안전 측.
        is_writable_v1: false,
    }
}

fn map_partition(
    raw: &MsftPartition,
    vols_by_letter: &HashMap<char, &MsftVolume>,
    vols_by_path: &HashMap<&str, &MsftVolume>,
    bitlocker_query_ok: bool,
    bitlocker_by_mount: &HashMap<String, BitLockerStatus>,
) -> Partition {
    let drive_letter_char = char_from_wmi_letter(raw.drive_letter.as_deref())
        .or_else(|| drive_letter_from_access_paths(raw.access_paths.as_deref()));
    let drive_letter = drive_letter_char.map(|c| c.to_string());

    // 볼륨 매칭: 드라이브 문자 → AccessPaths(\\?\Volume{...}\) 순.
    let volume = drive_letter_char
        .and_then(|c| vols_by_letter.get(&c).copied())
        .or_else(|| {
            raw.access_paths
                .as_ref()?
                .iter()
                .find_map(|ap| vols_by_path.get(ap.as_str()).copied())
        });

    let label = volume
        .and_then(|v| v.file_system_label.clone())
        .filter(|s| !s.is_empty());
    let file_system =
        file_system_from(volume.and_then(|v| v.file_system.as_deref()), &raw.gpt_type);
    let bitlocker_status = volume
        .and_then(|v| v.path.as_deref())
        .and_then(|path| {
            bitlocker_by_mount
                .get(&normalize_mount_point(path))
                .copied()
        })
        .or_else(|| {
            drive_letter.as_deref().and_then(|letter| {
                bitlocker_by_mount
                    .get(&normalize_mount_point(&format!("{letter}:")))
                    .copied()
            })
        })
        .unwrap_or(if bitlocker_query_ok {
            BitLockerStatus::NotEncrypted
        } else {
            BitLockerStatus::Unknown
        });

    let id = format!("disk{}-part{}", raw.disk_number, raw.partition_number);

    Partition {
        id,
        index: raw.partition_number,
        offset_bytes: raw.offset,
        size_bytes: raw.size,
        drive_letter,
        label,
        file_system,
        is_boot: raw.is_boot,
        is_system: raw.is_system,
        is_hidden: raw.is_hidden,
        bitlocker_status,
        // V1 정책: 드라이브 문자가 부여되어 마운트된 볼륨은 사용 중으로 간주.
        // 더 엄격한 핸들 검사는 safety 모듈에서 별도 가드로 추가한다.
        is_in_use: drive_letter_char.is_some(),
    }
}

fn query_bitlocker_statuses() -> (bool, HashMap<String, BitLockerStatus>) {
    let com = match COMLibrary::new() {
        Ok(com) => com,
        Err(error) => {
            warn!(target: "parq::platform", %error, "BitLocker COM 초기화 실패");
            return (false, HashMap::new());
        }
    };
    let conn = match WMIConnection::with_namespace_path(
        "ROOT\\CIMV2\\Security\\MicrosoftVolumeEncryption",
        com,
    ) {
        Ok(conn) => conn,
        Err(error) => {
            warn!(target: "parq::platform", %error, "BitLocker WMI 네임스페이스 연결 실패");
            return (false, HashMap::new());
        }
    };
    let volumes: Vec<Win32EncryptableVolume> = match conn.raw_query(
        "SELECT DeviceID, DriveLetter, ProtectionStatus, ConversionStatus \
         FROM Win32_EncryptableVolume",
    ) {
        Ok(volumes) => volumes,
        Err(error) => {
            warn!(target: "parq::platform", %error, "BitLocker 상태 쿼리 실패");
            return (false, HashMap::new());
        }
    };
    let mut by_mount = HashMap::new();
    for volume in volumes {
        let status = bitlocker_status(&volume);
        by_mount.insert(normalize_mount_point(&volume.device_id), status);
        if let Some(letter) = volume.drive_letter.as_deref().filter(|s| !s.is_empty()) {
            by_mount.insert(normalize_mount_point(letter), status);
        }
    }
    (true, by_mount)
}

fn bitlocker_status(volume: &Win32EncryptableVolume) -> BitLockerStatus {
    if volume.conversion_status == Some(0) && volume.protection_status == Some(0) {
        BitLockerStatus::NotEncrypted
    } else {
        BitLockerStatus::Encrypted
    }
}

fn normalize_mount_point(value: &str) -> String {
    value.trim().trim_end_matches('\\').to_ascii_uppercase()
}

/// WMI Char16 (`Option<String>` 으로 도착) 을 도메인용 ASCII 문자로 정규화.
/// 드라이브 문자가 없으면 wmi crate 가 "\0" 또는 빈 문자열을 주는데, 둘 다 None 으로 본다.
fn char_from_wmi_letter(letter: Option<&str>) -> Option<char> {
    let s = letter?;
    let c = s.chars().next()?;
    if c == '\0' || !c.is_ascii_alphabetic() {
        return None;
    }
    Some(c.to_ascii_uppercase())
}

fn drive_letter_from_access_paths(access_paths: Option<&[String]>) -> Option<char> {
    access_paths?.iter().find_map(|path| {
        let bytes = path.as_bytes();
        (bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic())
            .then(|| (bytes[0] as char).to_ascii_uppercase())
    })
}

fn file_system_from(name: Option<&str>, gpt_type: &Option<String>) -> FileSystemKind {
    if let Some(n) = name.map(|n| n.trim()) {
        let kind = match n.to_ascii_uppercase().as_str() {
            "NTFS" => Some(FileSystemKind::Ntfs),
            "FAT32" => Some(FileSystemKind::Fat32),
            "EXFAT" => Some(FileSystemKind::ExFat),
            "REFS" => Some(FileSystemKind::ReFs),
            "" => None,
            other => {
                debug!(target: "parq::platform", fs = other, "알 수 없는 파일시스템");
                None
            }
        };
        if let Some(k) = kind {
            return k;
        }
    }
    if let Some(g) = gpt_type {
        if g.eq_ignore_ascii_case(EFI_GPT_TYPE) {
            return FileSystemKind::Efi;
        }
        if g.eq_ignore_ascii_case(RECOVERY_GPT_TYPE) {
            return FileSystemKind::Ntfs;
        }
    }
    FileSystemKind::Unknown
}

/// MSFT_Disk.BusType 정수 → 도메인 enum 매핑.
/// 참조: https://learn.microsoft.com/en-us/windows-hardware/drivers/storage/msft-disk
fn bus_type_from_wmi(value: u16) -> BusType {
    match value {
        1 => BusType::Scsi,
        2 | 3 | 11 => BusType::Sata, // ATAPI / ATA / SATA — V1 에서는 동일하게 취급
        4 => BusType::Ieee1394,
        7 => BusType::Usb,
        12 => BusType::Sd,
        13 => BusType::Mmc,
        14 | 15 => BusType::Virtual,
        17 => BusType::Nvme,
        // RAID(8) / iSCSI(9) / SAS(10) / Storage Spaces(16) 는 SCSI 계열로 묶어 처리.
        8 | 9 | 10 | 16 => BusType::Scsi,
        other => {
            warn!(target: "parq::platform", bus_type = other, "알 수 없는 BusType");
            BusType::Unknown
        }
    }
}

fn partition_style_from_wmi(value: u16) -> PartitionStyle {
    match value {
        1 => PartitionStyle::Mbr,
        2 => PartitionStyle::Gpt,
        _ => PartitionStyle::Raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_letter_empty_or_null_is_none() {
        assert_eq!(char_from_wmi_letter(None), None);
        assert_eq!(char_from_wmi_letter(Some("")), None);
        assert_eq!(char_from_wmi_letter(Some("\0")), None);
    }

    #[test]
    fn drive_letter_decodes_ascii_and_uppercases() {
        assert_eq!(char_from_wmi_letter(Some("C")), Some('C'));
        assert_eq!(char_from_wmi_letter(Some("e")), Some('E'));
    }

    #[test]
    fn drive_letter_rejects_non_alphabetic() {
        assert_eq!(char_from_wmi_letter(Some(" ")), None);
        assert_eq!(char_from_wmi_letter(Some("0")), None);
    }

    #[test]
    fn drive_letter_falls_back_to_access_paths() {
        let paths = vec![r"\\?\Volume{test}\".into(), r"s:\".into()];
        assert_eq!(drive_letter_from_access_paths(Some(&paths)), Some('S'));
        assert_eq!(drive_letter_from_access_paths(Some(&paths[..1])), None);
        assert_eq!(drive_letter_from_access_paths(None), None);
    }

    #[test]
    fn bus_type_known_values() {
        assert_eq!(bus_type_from_wmi(7), BusType::Usb);
        assert_eq!(bus_type_from_wmi(11), BusType::Sata);
        assert_eq!(bus_type_from_wmi(17), BusType::Nvme);
        assert_eq!(bus_type_from_wmi(12), BusType::Sd);
        assert_eq!(bus_type_from_wmi(14), BusType::Virtual);
        assert_eq!(bus_type_from_wmi(15), BusType::Virtual);
        assert_eq!(bus_type_from_wmi(0), BusType::Unknown);
        assert_eq!(bus_type_from_wmi(255), BusType::Unknown);
    }

    #[test]
    fn partition_style_mapping() {
        assert_eq!(partition_style_from_wmi(1), PartitionStyle::Mbr);
        assert_eq!(partition_style_from_wmi(2), PartitionStyle::Gpt);
        assert_eq!(partition_style_from_wmi(0), PartitionStyle::Raw);
        assert_eq!(partition_style_from_wmi(99), PartitionStyle::Raw);
    }

    #[test]
    fn file_system_from_name_known() {
        assert_eq!(file_system_from(Some("NTFS"), &None), FileSystemKind::Ntfs);
        assert_eq!(
            file_system_from(Some("FAT32"), &None),
            FileSystemKind::Fat32
        );
        assert_eq!(
            file_system_from(Some("exFAT"), &None),
            FileSystemKind::ExFat
        );
        assert_eq!(file_system_from(Some("ReFS"), &None), FileSystemKind::ReFs);
    }

    #[test]
    fn file_system_efi_from_gpt_type_when_volume_missing() {
        let efi = Some(EFI_GPT_TYPE.to_string());
        assert_eq!(file_system_from(None, &efi), FileSystemKind::Efi);
        // 빈 파일시스템 이름이어도 GPT 타입으로 판정.
        assert_eq!(file_system_from(Some(""), &efi), FileSystemKind::Efi);
        // 대소문자 무시.
        let upper = Some(EFI_GPT_TYPE.to_uppercase());
        assert_eq!(file_system_from(None, &upper), FileSystemKind::Efi);
    }

    #[test]
    fn file_system_ntfs_from_recovery_gpt_type() {
        // Recovery 파티션은 보통 드라이브 문자가 없어 MSFT_Volume 매칭 실패하지만
        // GPT type 으로 NTFS 단정. 사용자에게 "Unknown" 으로 안 보이게.
        let recovery = Some(RECOVERY_GPT_TYPE.to_string());
        assert_eq!(file_system_from(None, &recovery), FileSystemKind::Ntfs);
        assert_eq!(file_system_from(Some(""), &recovery), FileSystemKind::Ntfs);
        let upper = Some(RECOVERY_GPT_TYPE.to_uppercase());
        assert_eq!(file_system_from(None, &upper), FileSystemKind::Ntfs);
    }

    #[test]
    fn file_system_unknown_when_no_match() {
        assert_eq!(file_system_from(None, &None), FileSystemKind::Unknown);
        assert_eq!(
            file_system_from(Some("NewFS"), &None),
            FileSystemKind::Unknown
        );
    }

    #[test]
    fn bitlocker_status_distinguishes_decrypted_and_encrypted() {
        let mut volume = Win32EncryptableVolume {
            device_id: r"\\?\Volume{test}\".into(),
            drive_letter: Some("E:".into()),
            protection_status: Some(0),
            conversion_status: Some(0),
        };
        assert_eq!(bitlocker_status(&volume), BitLockerStatus::NotEncrypted);

        volume.conversion_status = Some(1);
        volume.protection_status = Some(1);
        assert_eq!(bitlocker_status(&volume), BitLockerStatus::Encrypted);
    }

    #[test]
    fn bitlocker_mount_points_are_case_and_slash_insensitive() {
        assert_eq!(normalize_mount_point("e:\\"), "E:");
        assert_eq!(
            normalize_mount_point(r"\\?\Volume{ABC}\"),
            normalize_mount_point(r"\\?\volume{abc}")
        );
    }
}
