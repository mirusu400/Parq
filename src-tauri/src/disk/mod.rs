// 디스크 / 파티션 정보 조회. **read-only** 모듈.
// 여기에 쓰기 코드를 추가하지 말 것 — PR 거절 사유.
//
// 도메인 타입은 프론트엔드 src/types.ts 와 1:1 대응한다 (camelCase serde).
// 변경 시 양쪽을 함께 갱신할 것.

use serde::{Deserialize, Serialize};

use crate::Result;

/// 디스크 버스 타입. 안전 정책에서 외장 미디어 화이트리스트로 사용.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BusType {
    #[serde(rename = "USB")]
    Usb,
    #[serde(rename = "SD")]
    Sd,
    #[serde(rename = "MMC")]
    Mmc,
    #[serde(rename = "IEEE1394")]
    Ieee1394,
    #[serde(rename = "SATA")]
    Sata,
    #[serde(rename = "NVMe")]
    Nvme,
    #[serde(rename = "SCSI")]
    Scsi,
    Virtual,
    Unknown,
}

impl BusType {
    /// V1 외장 미디어 화이트리스트. docs/safety-model.md 참고.
    #[must_use]
    pub fn is_removable_class(self) -> bool {
        matches!(
            self,
            BusType::Usb | BusType::Sd | BusType::Mmc | BusType::Ieee1394
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionStyle {
    #[serde(rename = "MBR")]
    Mbr,
    #[serde(rename = "GPT")]
    Gpt,
    #[serde(rename = "RAW")]
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileSystemKind {
    #[serde(rename = "NTFS")]
    Ntfs,
    #[serde(rename = "FAT32")]
    Fat32,
    #[serde(rename = "exFAT")]
    ExFat,
    #[serde(rename = "ReFS")]
    ReFs,
    #[serde(rename = "EFI")]
    Efi,
    #[serde(rename = "Unknown")]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Disk {
    /// 안정적인 식별자 (시리얼 또는 디스크 GUID). 디스크 번호는 매번 바뀌므로 사용 금지.
    pub id: String,
    pub number: u32,
    pub model: String,
    pub serial: Option<String>,
    pub size_bytes: u64,
    pub bus_type: BusType,
    pub partition_style: PartitionStyle,
    pub is_removable: bool,
    pub is_system: bool,
    pub is_read_only: bool,
    pub partitions: Vec<Partition>,
    /// 현재 환경(`PARQ_DEV_ALLOW_INTERNAL_DISKS` 포함) 에서 V1 safety 가드를 통과하는지.
    /// `disk::enumerate` 가 채워준다 — 직접 만든 fixture 는 false 가 기본값.
    #[serde(default)]
    pub is_writable_v1: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Partition {
    pub id: String,
    pub index: u32,
    /// 디스크 시작으로부터의 바이트 오프셋.
    pub offset_bytes: u64,
    pub size_bytes: u64,
    pub drive_letter: Option<String>,
    pub label: Option<String>,
    pub file_system: FileSystemKind,
    pub is_boot: bool,
    pub is_system: bool,
    pub is_hidden: bool,
    /// 마운트되어 사용 중이면 true — V1 에서 쓰기 작업 차단의 1차 신호.
    pub is_in_use: bool,
}

/// 시스템에 연결된 모든 디스크와 파티션을 열거한다. **read-only**.
///
/// 플랫폼 구현은 `platform::wmi` 에 위임하고, 그 결과에 V1 safety 검증을 후처리해
/// `is_writable_v1` 을 채운다. 비-Windows 빌드는 `NotImplemented` 를 반환한다.
pub fn enumerate() -> Result<Vec<Disk>> {
    #[cfg(windows)]
    {
        let mut disks = crate::platform::wmi::enumerate_disks()?;
        for d in &mut disks {
            d.is_writable_v1 = crate::safety::check_disk_writable(d).is_ok();
        }
        Ok(disks)
    }
    #[cfg(not(windows))]
    {
        Err(crate::ParqError::NotImplemented(
            "disk::enumerate (non-Windows)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_type_removable_class() {
        assert!(BusType::Usb.is_removable_class());
        assert!(BusType::Sd.is_removable_class());
        assert!(BusType::Mmc.is_removable_class());
        assert!(BusType::Ieee1394.is_removable_class());
        assert!(!BusType::Sata.is_removable_class());
        assert!(!BusType::Nvme.is_removable_class());
        assert!(!BusType::Virtual.is_removable_class());
        assert!(!BusType::Unknown.is_removable_class());
    }

    #[test]
    fn disk_serializes_camel_case() {
        let disk = Disk {
            id: "disk-1".into(),
            number: 1,
            model: "Test".into(),
            serial: Some("SN1".into()),
            size_bytes: 1024,
            bus_type: BusType::Usb,
            partition_style: PartitionStyle::Gpt,
            is_removable: true,
            is_system: false,
            is_read_only: false,
            partitions: vec![],
            is_writable_v1: false,
        };
        let json = serde_json::to_string(&disk).expect("serialize");
        // 프론트엔드 types.ts 와 호환되는 camelCase 키 확인.
        assert!(json.contains("\"sizeBytes\":1024"));
        assert!(json.contains("\"busType\":\"USB\""));
        assert!(json.contains("\"partitionStyle\":\"GPT\""));
        assert!(json.contains("\"isRemovable\":true"));
    }

    #[test]
    fn partition_serializes_camel_case() {
        let p = Partition {
            id: "p-1".into(),
            index: 1,
            offset_bytes: 1_048_576,
            size_bytes: 2048,
            drive_letter: Some("E".into()),
            label: None,
            file_system: FileSystemKind::ExFat,
            is_boot: false,
            is_system: false,
            is_hidden: false,
            is_in_use: false,
        };
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains("\"offsetBytes\":1048576"));
        assert!(json.contains("\"driveLetter\":\"E\""));
        assert!(json.contains("\"fileSystem\":\"exFAT\""));
        assert!(json.contains("\"isInUse\":false"));
    }
}
