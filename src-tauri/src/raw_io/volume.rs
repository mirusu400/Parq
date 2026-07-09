//! 볼륨 lock / dismount 래퍼 (RAII). `docs/v2-raw-io.md §4`.
//!
//! raw write 전에 대상 볼륨을 **독점(lock)** 하고 파일시스템 드라이버를 **분리(dismount)** 한다.
//! 그래야 우리가 `\\.\PhysicalDriveN` 에 섹터를 쓰는 동안 FS 드라이버가 같은 섹터를 건드려
//! 손상시키는 일을 막는다.
//!
//! **이 모듈은 디스크 데이터를 write 하지 않는다** — `FSCTL_LOCK_VOLUME` / `FSCTL_DISMOUNT_VOLUME`
//! / `FSCTL_UNLOCK_VOLUME` 제어 호출만 한다. 실제 섹터 write(`WriteFile`)는 별도 PR(charter §3-5,
//! Phase 3). lock/dismount 는 되돌릴 수 있는 작업이지만 볼륨을 마운트 해제하므로, 알파 게이트
//! (`safety::require_v2_destructive`) 통과를 요구한다.
#![deny(unsafe_op_in_unsafe_fn)]

use tracing::{info, instrument};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME,
};
use windows::Win32::System::IO::DeviceIoControl;

use super::map_win_err;
use crate::{safety, ParqError, Result};

/// lock + dismount 된 볼륨 핸들. `Drop` 이 unlock + `CloseHandle` 을 보장한다(RAII).
///
/// 이 핸들은 read/write 접근으로 열리지만, 이 타입은 어떤 `WriteFile` 도 노출/수행하지 않는다.
pub struct VolumeLock {
    handle: HANDLE,
    letter: String,
    locked: bool,
}

impl VolumeLock {
    /// 드라이브 문자(`"E"` 또는 `"E:"`)의 볼륨을 열어 lock + dismount 한다.
    ///
    /// - 알파 게이트(`PARQ_ENABLE_V2_DESTRUCTIVE`) 통과 필수.
    /// - lock 실패(다른 열린 핸들 존재 = 볼륨 사용 중) 시 강제하지 않고 에러 반환(charter §2).
    #[instrument]
    pub fn lock_and_dismount(drive_letter: &str) -> Result<VolumeLock> {
        safety::require_v2_destructive()?;

        let letter = drive_letter.trim().trim_end_matches(':');
        let is_single_alpha = letter.len() == 1
            && letter
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic());
        if !is_single_alpha {
            return Err(ParqError::ValidationFailed(format!(
                "잘못된 드라이브 문자: {drive_letter:?}"
            )));
        }
        let letter = letter.to_ascii_uppercase();
        let path = format!(r"\\.\{letter}:");
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

        // SAFETY: wide 는 NUL 종단 UTF-16. RW 핸들이지만 이 래퍼는 WriteFile 을 절대 호출하지
        //         않는다 — FSCTL 제어만. 반환 핸들은 즉시 VolumeLock(RAII)로 감싼다.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                HANDLE::default(),
            )
        }
        .map_err(|e| map_win_err(&format!("볼륨 {letter}: open"), &e))?;

        // locked=false 로 시작 → lock 성공 전에 에러가 나면 Drop 이 핸들만 close(unlock 안 함).
        let mut lock = VolumeLock {
            handle,
            letter: letter.clone(),
            locked: false,
        };

        fsctl(handle, FSCTL_LOCK_VOLUME, "FSCTL_LOCK_VOLUME")?;
        lock.locked = true;

        fsctl(handle, FSCTL_DISMOUNT_VOLUME, "FSCTL_DISMOUNT_VOLUME")?;

        info!(target: "parq::raw_io", %letter, "볼륨 lock + dismount 완료");
        Ok(lock)
    }

    /// 락된 드라이브 문자(대문자, `:` 없음).
    #[must_use]
    pub fn letter(&self) -> &str {
        &self.letter
    }
}

impl Drop for VolumeLock {
    fn drop(&mut self) {
        if self.locked {
            // SAFETY: 유효 핸들. unlock 은 lock 의 역연산. Drop 경로라 에러는 무시.
            let _ = fsctl(self.handle, FSCTL_UNLOCK_VOLUME, "FSCTL_UNLOCK_VOLUME");
        }
        // SAFETY: CreateFileW 성공으로 얻은 유효 핸들. Drop 은 1회 → 한 번만 close.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// 입력/출력 버퍼 없는 FSCTL 제어 호출. 데이터 write 아님.
fn fsctl(handle: HANDLE, code: u32, name: &str) -> Result<()> {
    let mut returned: u32 = 0;
    // SAFETY: in/out 버퍼 없음(None, 0). 유효 핸들. 제어 코드만 전달.
    unsafe { DeviceIoControl(handle, code, None, 0, None, 0, Some(&mut returned), None) }
        .map_err(|e| map_win_err(name, &e))
}
