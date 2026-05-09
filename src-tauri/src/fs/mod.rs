// 파일시스템 작업 (포맷 / 라벨 변경).
//
// V1 에서는 직접 메타데이터를 쓰지 않고 platform::powershell 을 거친다.
// 파괴적 작업의 4단계 (plan/validate/preview/execute) 자체는 호출자 (`partition::execute_*`,
// `commands/write.rs`) 가 책임지고, 이 모듈은 검증된 단일 동작만 노출한다.

use crate::disk::FileSystemKind;
use crate::platform::powershell;
use crate::{ParqError, Result};

/// V1 가 지원하는 포맷 대상 파일시스템.
///
/// 다른 변형 (ReFS, EFI 등) 은 V1 에서 거부한다.
fn fs_to_powershell(fs: FileSystemKind) -> Result<&'static str> {
    match fs {
        FileSystemKind::Fat32 => Ok("FAT32"),
        FileSystemKind::ExFat => Ok("exFAT"),
        FileSystemKind::Ntfs => Ok("NTFS"),
        other => Err(ParqError::ValidationFailed(format!(
            "V1 은 FAT32 / exFAT / NTFS 만 지원합니다 — 요청: {other:?}"
        ))),
    }
}

/// 파일시스템별 라벨 길이 / 문자 제한 검증.
///
/// FAT32: 11자 이내. exFAT: 15자 이내. NTFS: 32자 이내.
/// 모두 ASCII 영숫자 + 공백 + `-` `_` 만 허용 (V1 보수적 정책 — 인코딩 이슈 회피).
pub fn validate_label(label: &str, fs: FileSystemKind) -> Result<()> {
    let max = match fs {
        FileSystemKind::Fat32 => 11,
        FileSystemKind::ExFat => 15,
        FileSystemKind::Ntfs => 32,
        other => {
            return Err(ParqError::ValidationFailed(format!(
                "V1 은 FAT32 / exFAT / NTFS 만 지원합니다 — 요청: {other:?}"
            )))
        }
    };
    if label.chars().count() > max {
        return Err(ParqError::ValidationFailed(format!(
            "라벨이 너무 깁니다: {} 자 (최대 {max}자, {fs:?})",
            label.chars().count()
        )));
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-' || c == '_')
    {
        return Err(ParqError::ValidationFailed(
            "라벨은 ASCII 영숫자, 공백, '-', '_' 만 허용합니다 (V1)".into(),
        ));
    }
    Ok(())
}

/// 드라이브 문자로 식별된 볼륨을 포맷한다. 라벨은 옵션 — 지정 시 `validate_label` 를 통과해야 한다.
///
/// 호출자가 `safety::check_partition_destructive` 를 통과시킨 후, transaction step 안에서
/// 호출하는 것을 가정한다. 이 함수 자체는 안전 가드를 다시 호출하지 않는다.
pub fn format_volume(letter: &str, fs: FileSystemKind, label: Option<&str>) -> Result<()> {
    if !is_single_drive_letter(letter) {
        return Err(ParqError::ValidationFailed(format!(
            "유효하지 않은 드라이브 문자: {letter:?}"
        )));
    }
    let fs_name = fs_to_powershell(fs)?;
    if let Some(l) = label {
        validate_label(l, fs)?;
    }

    let label_arg = label
        .map(|l| format!(" -NewFileSystemLabel {}", powershell::quote_single(l)))
        .unwrap_or_default();
    let script = format!(
        "Format-Volume -DriveLetter {letter} -FileSystem {fs_name}{label_arg} \
         -Confirm:$false -Force | Out-Null"
    );
    powershell::run_command(&script)?;
    Ok(())
}

/// 마운트된 볼륨의 라벨만 변경한다 (포맷 없이). `Set-Volume -NewFileSystemLabel`.
pub fn set_volume_label(letter: &str, fs: FileSystemKind, label: &str) -> Result<()> {
    if !is_single_drive_letter(letter) {
        return Err(ParqError::ValidationFailed(format!(
            "유효하지 않은 드라이브 문자: {letter:?}"
        )));
    }
    validate_label(label, fs)?;
    let script = format!(
        "Set-Volume -DriveLetter {letter} -NewFileSystemLabel {}",
        powershell::quote_single(label)
    );
    powershell::run_command(&script)?;
    Ok(())
}

fn is_single_drive_letter(s: &str) -> bool {
    s.len() == 1 && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_label_fat32_max_length() {
        assert!(validate_label("12345678901", FileSystemKind::Fat32).is_ok()); // 11
        assert!(validate_label("123456789012", FileSystemKind::Fat32).is_err()); // 12
    }

    #[test]
    fn validate_label_ntfs_allows_longer() {
        assert!(validate_label(&"X".repeat(32), FileSystemKind::Ntfs).is_ok());
        assert!(validate_label(&"X".repeat(33), FileSystemKind::Ntfs).is_err());
    }

    #[test]
    fn validate_label_rejects_non_ascii() {
        assert!(validate_label("한글라벨", FileSystemKind::Ntfs).is_err());
    }

    #[test]
    fn validate_label_allows_spaces_and_underscore_and_dash() {
        assert!(validate_label("MY-DISK_01", FileSystemKind::Ntfs).is_ok());
        assert!(validate_label("a b c", FileSystemKind::Ntfs).is_ok());
    }

    #[test]
    fn validate_label_rejects_unsupported_fs() {
        assert!(validate_label("X", FileSystemKind::ReFs).is_err());
        assert!(validate_label("X", FileSystemKind::Efi).is_err());
        assert!(validate_label("X", FileSystemKind::Unknown).is_err());
    }

    #[test]
    fn drive_letter_validator() {
        assert!(is_single_drive_letter("C"));
        assert!(is_single_drive_letter("e"));
        assert!(!is_single_drive_letter("CC"));
        assert!(!is_single_drive_letter("C:"));
        assert!(!is_single_drive_letter(""));
        assert!(!is_single_drive_letter("1"));
    }
}
