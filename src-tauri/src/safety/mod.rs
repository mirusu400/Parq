// 안전 가드 / 검증 로직.
//
// V1 정책: docs/safety-model.md 참고.
// - 외장 미디어 (USB/SD/MMC/IEEE1394 또는 IsRemovable) 만 쓰기 허용
// - 시스템 디스크 작업은 현재 부팅 NTFS 볼륨의 Windows 온라인 리사이즈만 예외 허용
// - 파괴적 작업 (포맷/삭제) 은 마운트 해제된 파티션만 허용
// - V1 에는 사용자 대상 우회 플래그 (--force 등) 를 절대 추가하지 않는다
//
// 개발 전용 우회: PARQ_DEV_ALLOW_INTERNAL_DISKS=1 환경 변수가 설정되어 있으면 외장-아님
// 디스크의 bus-type 가드만 우회한다 (시스템/읽기전용/부팅/시스템 파티션 가드는 계속 유효).
// 이는 VM 내부 NVMe / VHD 같은 개발 환경에서만 사용 — 일반 사용자가 실수로 켤 수 없도록
// 의도적으로 환경변수 명을 길게 잡았다. UI 노출 절대 금지.
//
// 이 모듈은 read-only 다 — 디스크에 어떤 쓰기도 하지 않는다.

use tracing::{instrument, warn};

use crate::disk::{BitLockerStatus, Disk, FileSystemKind, Partition};
use crate::{ParqError, Result};

const DEV_BYPASS_ENV: &str = "PARQ_DEV_ALLOW_INTERNAL_DISKS";

/// V2 destructive 기능(파티션 이동 / raw write) 알파 게이트 환경변수. docs/v2-charter.md §3-1·§5.
const V2_DESTRUCTIVE_ENV: &str = "PARQ_ENABLE_V2_DESTRUCTIVE";
const OFFLINE_SYSTEM_MOVE_ENV: &str = "PARQ_ENABLE_OFFLINE_SYSTEM_MOVE";

fn env_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// 개발 전용 bus-type 가드 우회가 환경변수로 활성화되어 있는지 확인.
/// "1" 또는 "true" (대소문자 무시) 면 활성. 그 외에는 비활성.
fn dev_bypass_enabled() -> bool {
    env_enabled(DEV_BYPASS_ENV)
}

#[must_use]
pub fn is_windows_pe() -> bool {
    #[cfg(windows)]
    {
        let system_drive_is_x = std::env::var("SystemDrive")
            .map(|drive| drive.eq_ignore_ascii_case("X:"))
            .unwrap_or(false);
        let winpe_binary = std::path::Path::new(r"X:\Windows\System32\wpeutil.exe").is_file();
        let mini_nt_key = std::process::Command::new("reg.exe")
            .args(["query", r"HKLM\SYSTEM\CurrentControlSet\Control\MiniNT"])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        system_drive_is_x && winpe_binary && mini_nt_key
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[must_use]
pub fn offline_system_move_enabled() -> bool {
    env_enabled(OFFLINE_SYSTEM_MOVE_ENV) && is_windows_pe()
}

pub fn require_offline_system_move() -> Result<()> {
    require_v2_destructive()?;
    if !offline_system_move_enabled() {
        return Err(ParqError::ValidationFailed(format!(
            "오프라인 시스템 이동은 실제 Windows PE에서 {OFFLINE_SYSTEM_MOVE_ENV}=1 이 설정된 경우에만 허용됩니다"
        )));
    }
    Ok(())
}

/// V1 외장 미디어 화이트리스트.
///
/// BusType 이 USB/SD/MMC/IEEE1394 이거나, MSFT_Disk 가 IsRemovable=true 로 보고한 디스크만 통과.
/// 내부 SATA/NVMe 는 V2 이후 별도 검증을 거쳐 확장한다.
#[must_use]
pub fn is_external_media(disk: &Disk) -> bool {
    disk.bus_type.is_removable_class() || disk.is_removable
}

/// 디스크 자체가 V1 쓰기 화이트리스트를 통과하는지 검증.
///
/// 거부 조건:
/// 1. 시스템 디스크 (`MSFT_Disk.IsSystem` 또는 `IsBoot`)
/// 2. 디스크가 읽기 전용으로 표시됨
/// 3. 외장 미디어가 아님
///
/// 모든 파괴적 작업의 1차 가드. 하위 단계 (`check_partition_*`) 는 이 함수를 호출한다.
#[instrument(skip(disk), fields(disk_id = %disk.id, disk_number = disk.number))]
pub fn check_disk_writable(disk: &Disk) -> Result<()> {
    let offline_system_move = offline_system_move_enabled();
    if disk.is_system && !offline_system_move {
        return Err(ParqError::SystemPartitionProtected(format!(
            "디스크 {} ({}) 은 시스템 디스크입니다 — V1 에서 차단됨",
            disk.number, disk.model
        )));
    }
    if disk.is_read_only {
        return Err(ParqError::ValidationFailed(format!(
            "디스크 {} ({}) 은 읽기 전용 모드입니다",
            disk.number, disk.model
        )));
    }
    if !is_external_media(disk) {
        if offline_system_move {
            warn!(
                target: "parq::safety",
                disk_id = %disk.id,
                bus_type = ?disk.bus_type,
                is_system = disk.is_system,
                "Windows PE 오프라인 시스템 이동 게이트 활성화 — 내부/시스템 디스크 가드 우회"
            );
            return Ok(());
        }
        if dev_bypass_enabled() {
            warn!(
                target: "parq::safety",
                disk_id = %disk.id,
                bus_type = ?disk.bus_type,
                "{DEV_BYPASS_ENV} 활성화 — bus-type 가드 우회 (시스템/읽기전용 가드는 유지)"
            );
            return Ok(());
        }
        return Err(ParqError::ValidationFailed(format!(
            "디스크 {} ({}) 은 외장 미디어가 아닙니다 — V1 은 USB/SD/MMC/IEEE1394 또는 \
             제거 가능 디스크만 지원합니다 (BusType={:?})",
            disk.number, disk.model, disk.bus_type
        )));
    }
    Ok(())
}

/// 파티션의 메타데이터 변경 (라벨 변경 등) 이 허용되는지 검증.
///
/// `check_disk_writable` + 부팅/시스템 파티션 차단. 마운트 여부는 허용한다 — 데이터를 직접
/// 손상시키지 않는 작업용 (예: PowerShell `Set-Volume -NewFileSystemLabel`).
#[instrument(skip(disk, partition), fields(disk_id = %disk.id, partition_id = %partition.id))]
pub fn check_partition_metadata_writable(disk: &Disk, partition: &Partition) -> Result<()> {
    check_disk_writable(disk)?;
    if partition.is_boot {
        return Err(ParqError::SystemPartitionProtected(format!(
            "파티션 {} 은 부팅 파티션입니다",
            partition.id
        )));
    }
    if partition.is_system {
        return Err(ParqError::SystemPartitionProtected(format!(
            "파티션 {} 은 시스템 플래그가 설정되어 있습니다",
            partition.id
        )));
    }
    check_bitlocker_writable(partition)
}

fn check_bitlocker_writable(partition: &Partition) -> Result<()> {
    match partition.bitlocker_status {
        BitLockerStatus::NotEncrypted => {}
        BitLockerStatus::Encrypted => {
            return Err(ParqError::ValidationFailed(format!(
            "파티션 {} 은 BitLocker 암호화 상태입니다 — 보호를 완전히 해제한 뒤 다시 시도하세요",
            partition.id
        )))
        }
        BitLockerStatus::Unknown => {
            return Err(ParqError::ValidationFailed(format!(
                "파티션 {} 의 BitLocker 상태를 확인할 수 없어 안전을 위해 쓰기를 차단했습니다",
                partition.id
            )))
        }
    }
    Ok(())
}

/// NTFS 리사이즈 전용 가드.
///
/// 일반 파티션은 기존 V1 정책대로 마운트 해제를 요구한다. 시스템 디스크에서는 Windows가
/// 공식 지원하는 현재 부팅 NTFS 파티션의 온라인 리사이즈만 예외적으로 허용한다. EFI/MSR/
/// Recovery 등 다른 시스템 디스크 파티션과 BitLocker 볼륨은 계속 차단한다.
#[instrument(skip(disk, partition), fields(disk_id = %disk.id, partition_id = %partition.id))]
pub fn check_partition_resize_writable(disk: &Disk, partition: &Partition) -> Result<()> {
    if !disk.is_system {
        return check_partition_destructive(disk, partition);
    }
    if disk.is_read_only {
        return Err(ParqError::ValidationFailed(format!(
            "시스템 디스크 {} ({}) 은 읽기 전용 모드입니다",
            disk.number, disk.model
        )));
    }
    if !partition.is_boot
        || partition.is_system
        || partition.drive_letter.is_none()
        || partition.file_system != FileSystemKind::Ntfs
    {
        return Err(ParqError::SystemPartitionProtected(format!(
            "시스템 디스크에서는 현재 부팅 중인 NTFS 볼륨만 온라인 리사이즈할 수 있습니다: {}",
            partition.id
        )));
    }
    check_bitlocker_writable(partition)
}

/// 파티션의 데이터를 파괴하는 작업 (포맷 / 삭제 / 리사이즈) 이 허용되는지 검증.
///
/// `check_partition_metadata_writable` + 마운트 해제 요구.
/// V1 은 활성 핸들 스캔 대신 "드라이브 문자가 부여되어 있다 = 사용 중" 으로 간주한다.
/// 더 엄격한 핸들 스캔은 V2 에서 추가.
#[instrument(skip(disk, partition), fields(disk_id = %disk.id, partition_id = %partition.id))]
pub fn check_partition_destructive(disk: &Disk, partition: &Partition) -> Result<()> {
    check_partition_metadata_writable(disk, partition)?;
    if partition.is_in_use {
        return Err(ParqError::ValidationFailed(format!(
            "파티션 {} 은 현재 마운트되어 사용 중입니다 — 마운트 해제 후 다시 시도하세요",
            partition.id
        )));
    }
    Ok(())
}

#[instrument(skip(disk, partition), fields(disk_id = %disk.id, partition_id = %partition.id))]
pub fn check_partition_offline_system_move(disk: &Disk, partition: &Partition) -> Result<()> {
    check_partition_offline_system_move_lockable(disk, partition)?;
    if partition.is_in_use {
        return Err(ParqError::ValidationFailed(format!(
            "오프라인 이동 대상 {} 이 마운트되어 있습니다 — drive letter/access path를 제거하세요",
            partition.id
        )));
    }
    Ok(())
}

#[instrument(skip(disk, partition), fields(disk_id = %disk.id, partition_id = %partition.id))]
pub fn check_partition_offline_system_move_lockable(
    disk: &Disk,
    partition: &Partition,
) -> Result<()> {
    require_offline_system_move()?;
    if disk.partition_style != crate::disk::PartitionStyle::Gpt {
        return Err(ParqError::ValidationFailed(
            "오프라인 Windows 파티션 이동은 GPT 디스크만 지원합니다".into(),
        ));
    }
    check_disk_writable(disk)?;
    if partition.is_boot || partition.is_system {
        return Err(ParqError::SystemPartitionProtected(format!(
            "현재 WinPE 부팅/시스템 파티션은 이동할 수 없습니다: {}",
            partition.id
        )));
    }
    if !matches!(
        partition.file_system,
        FileSystemKind::Ntfs | FileSystemKind::Unknown
    ) {
        return Err(ParqError::ValidationFailed(
            "오프라인 Windows 이동 대상은 NTFS여야 합니다".into(),
        ));
    }
    check_bitlocker_writable(partition)
}

/// V2 destructive 기능 알파 게이트가 켜져 있는지. `PARQ_ENABLE_V2_DESTRUCTIVE` 가 "1" 또는
/// "true"(대소문자 무시) 일 때만 true. docs/v2-charter.md §3-1·§5.
///
/// **V1 기능엔 전혀 영향 없다** — 이 게이트는 V2 파티션 이동 / raw write 경로만 판단한다.
/// UI 노출 금지 (charter §3-1: 우연한 활성화 차단). `PARQ_DEV_ALLOW_INTERNAL_DISKS` 와 독립.
#[must_use]
pub fn v2_enabled() -> bool {
    env_enabled(V2_DESTRUCTIVE_ENV)
}

/// V2 destructive 경로 진입 가드. 게이트가 꺼져 있으면 즉시 거부한다.
///
/// 모든 V2 파티션 이동 / raw write 커맨드는 실제 작업 전에 이 함수를 통과해야 한다
/// (charter §3-1). 우회 플래그는 제공하지 않는다 (charter §2 비목표).
#[instrument]
pub fn require_v2_destructive() -> Result<()> {
    if !v2_enabled() {
        warn!(
            target: "parq::safety",
            "V2 destructive 요청이 거부됨 — {V2_DESTRUCTIVE_ENV} 미설정 (알파 게이트)"
        );
        return Err(ParqError::ValidationFailed(format!(
            "V2 destructive 기능이 비활성화되어 있습니다 — 활성화하려면 {V2_DESTRUCTIVE_ENV}=1 \
             환경변수가 필요합니다 (알파 게이트)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::{BusType, FileSystemKind, PartitionStyle};
    use crate::test_support::ENV_LOCK;

    fn make_disk(bus_type: BusType) -> Disk {
        Disk {
            id: "disk-test".into(),
            number: 1,
            model: "Test Disk".into(),
            serial: Some("SN".into()),
            size_bytes: 32_000_000_000,
            bus_type,
            partition_style: PartitionStyle::Mbr,
            is_removable: false,
            is_system: false,
            is_read_only: false,
            partitions: vec![],
            is_writable_v1: false,
        }
    }

    fn make_partition() -> Partition {
        Partition {
            id: "disk1-part1".into(),
            index: 1,
            offset_bytes: 1_048_576,
            size_bytes: 1_000_000_000,
            drive_letter: Some("E".into()),
            label: None,
            file_system: FileSystemKind::ExFat,
            is_boot: false,
            is_system: false,
            is_hidden: false,
            bitlocker_status: BitLockerStatus::NotEncrypted,
            is_in_use: true,
        }
    }

    #[test]
    fn external_media_usb_sd_mmc_ieee1394_pass() {
        for bus in [BusType::Usb, BusType::Sd, BusType::Mmc, BusType::Ieee1394] {
            assert!(
                is_external_media(&make_disk(bus)),
                "{bus:?} 는 외장이어야 함"
            );
        }
    }

    #[test]
    fn external_media_internal_buses_fail() {
        for bus in [BusType::Sata, BusType::Nvme, BusType::Scsi] {
            assert!(!is_external_media(&make_disk(bus)), "{bus:?} 는 외장 아님");
        }
    }

    #[test]
    fn external_media_falls_back_to_is_removable_flag() {
        let mut disk = make_disk(BusType::Unknown);
        assert!(!is_external_media(&disk));
        disk.is_removable = true;
        assert!(is_external_media(&disk));
    }

    #[test]
    fn check_disk_allows_usb() {
        let disk = make_disk(BusType::Usb);
        assert!(check_disk_writable(&disk).is_ok());
    }

    #[test]
    fn check_disk_denies_system_disk_even_if_usb() {
        // Windows To Go 같은 경우. is_system 이 우선.
        let mut disk = make_disk(BusType::Usb);
        disk.is_system = true;
        let err = check_disk_writable(&disk).unwrap_err();
        assert!(matches!(err, ParqError::SystemPartitionProtected(_)));
    }

    #[test]
    fn check_disk_denies_read_only() {
        let mut disk = make_disk(BusType::Usb);
        disk.is_read_only = true;
        let err = check_disk_writable(&disk).unwrap_err();
        assert!(matches!(err, ParqError::ValidationFailed(_)));
        assert!(err.to_string().contains("읽기 전용"));
    }

    #[test]
    fn check_disk_denies_internal_nvme() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(DEV_BYPASS_ENV);
        let disk = make_disk(BusType::Nvme);
        let err = check_disk_writable(&disk).unwrap_err();
        assert!(matches!(err, ParqError::ValidationFailed(_)));
        assert!(err.to_string().contains("외장 미디어가 아닙니다"));
    }

    #[test]
    fn dev_bypass_allows_internal_nvme_but_not_system_disk() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(DEV_BYPASS_ENV, "1");

        let disk = make_disk(BusType::Nvme);
        let allowed = check_disk_writable(&disk);

        let mut sys_disk = make_disk(BusType::Nvme);
        sys_disk.is_system = true;
        let denied = check_disk_writable(&sys_disk);

        std::env::remove_var(DEV_BYPASS_ENV);

        assert!(allowed.is_ok(), "bypass 활성 시 NVMe 통과해야 함");
        assert!(
            matches!(denied, Err(ParqError::SystemPartitionProtected(_))),
            "bypass 활성이어도 시스템 디스크는 거부되어야 함"
        );
    }

    #[test]
    fn metadata_change_allowed_on_mounted_usb_partition() {
        // 라벨 변경 같은 작업은 마운트 상태에서도 허용.
        let disk = make_disk(BusType::Usb);
        let mut p = make_partition();
        p.is_in_use = true;
        assert!(check_partition_metadata_writable(&disk, &p).is_ok());
    }

    #[test]
    fn metadata_change_denied_on_boot_partition() {
        let disk = make_disk(BusType::Usb);
        let mut p = make_partition();
        p.is_boot = true;
        let err = check_partition_metadata_writable(&disk, &p).unwrap_err();
        assert!(matches!(err, ParqError::SystemPartitionProtected(_)));
    }

    #[test]
    fn metadata_change_denied_on_system_partition() {
        let disk = make_disk(BusType::Usb);
        let mut p = make_partition();
        p.is_system = true;
        let err = check_partition_metadata_writable(&disk, &p).unwrap_err();
        assert!(matches!(err, ParqError::SystemPartitionProtected(_)));
    }

    #[test]
    fn metadata_change_denied_for_bitlocker_and_unknown_status() {
        let disk = make_disk(BusType::Usb);
        for status in [BitLockerStatus::Encrypted, BitLockerStatus::Unknown] {
            let mut partition = make_partition();
            partition.bitlocker_status = status;
            let err = check_partition_metadata_writable(&disk, &partition).unwrap_err();
            assert!(err.to_string().contains("BitLocker"));
        }
    }

    #[test]
    fn destructive_denied_when_partition_mounted() {
        let disk = make_disk(BusType::Usb);
        let mut p = make_partition();
        p.is_in_use = true;
        let err = check_partition_destructive(&disk, &p).unwrap_err();
        assert!(matches!(err, ParqError::ValidationFailed(_)));
        assert!(err.to_string().contains("마운트"));
    }

    #[test]
    fn destructive_allowed_on_unmounted_usb_partition() {
        let disk = make_disk(BusType::Usb);
        let mut p = make_partition();
        p.is_in_use = false;
        p.drive_letter = None;
        assert!(check_partition_destructive(&disk, &p).is_ok());
    }

    #[test]
    fn system_boot_ntfs_resize_is_allowed_but_other_system_partitions_are_not() {
        let mut disk = make_disk(BusType::Nvme);
        disk.is_system = true;

        let mut windows = make_partition();
        windows.drive_letter = Some("C".into());
        windows.file_system = FileSystemKind::Ntfs;
        windows.is_boot = true;
        windows.is_system = false;
        windows.is_in_use = true;
        assert!(check_partition_resize_writable(&disk, &windows).is_ok());

        let mut efi = windows.clone();
        efi.is_boot = false;
        efi.is_system = true;
        efi.file_system = FileSystemKind::Efi;
        assert!(matches!(
            check_partition_resize_writable(&disk, &efi),
            Err(ParqError::SystemPartitionProtected(_))
        ));
    }

    #[test]
    fn system_resize_rejects_bitlocker_and_read_only_disk() {
        let mut disk = make_disk(BusType::Nvme);
        disk.is_system = true;
        let mut windows = make_partition();
        windows.drive_letter = Some("C".into());
        windows.file_system = FileSystemKind::Ntfs;
        windows.is_boot = true;
        windows.is_in_use = true;

        windows.bitlocker_status = BitLockerStatus::Encrypted;
        assert!(check_partition_resize_writable(&disk, &windows).is_err());

        windows.bitlocker_status = BitLockerStatus::NotEncrypted;
        disk.is_read_only = true;
        assert!(check_partition_resize_writable(&disk, &windows).is_err());
    }

    #[test]
    fn destructive_inherits_disk_check() {
        // 디스크 가드를 통과 못하면 destructive 도 즉시 실패해야 함.
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(DEV_BYPASS_ENV);
        let disk = make_disk(BusType::Nvme);
        let mut p = make_partition();
        p.is_in_use = false;
        let err = check_partition_destructive(&disk, &p).unwrap_err();
        assert!(matches!(err, ParqError::ValidationFailed(_)));
        assert!(err.to_string().contains("외장 미디어가 아닙니다"));
    }

    #[test]
    fn v2_gate_off_by_default_rejects() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(V2_DESTRUCTIVE_ENV);
        assert!(!v2_enabled());
        let err = require_v2_destructive().unwrap_err();
        assert!(matches!(err, ParqError::ValidationFailed(_)));
        assert!(err.to_string().contains(V2_DESTRUCTIVE_ENV));
    }

    #[test]
    fn v2_gate_on_allows() {
        let _guard = ENV_LOCK.lock().unwrap();
        for val in ["1", "true", "TRUE", "True"] {
            std::env::set_var(V2_DESTRUCTIVE_ENV, val);
            assert!(v2_enabled(), "{val:?} 는 게이트를 켜야 함");
            assert!(require_v2_destructive().is_ok());
        }
        std::env::remove_var(V2_DESTRUCTIVE_ENV);
    }

    #[test]
    fn v2_gate_rejects_bogus_values() {
        let _guard = ENV_LOCK.lock().unwrap();
        for val in ["0", "false", "yes", "", "2"] {
            std::env::set_var(V2_DESTRUCTIVE_ENV, val);
            assert!(!v2_enabled(), "{val:?} 는 게이트를 켜면 안 됨");
        }
        std::env::remove_var(V2_DESTRUCTIVE_ENV);
    }
}
