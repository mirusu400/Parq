//! 볼륨 lock / dismount 래퍼 (RAII). `docs/v2-raw-io.md §4`.
//!
//! raw write 전에 대상 볼륨을 **독점(lock)** 하고 파일시스템 드라이버를 **분리(dismount)** 한다.
//! 그래야 우리가 `\\.\PhysicalDriveN` 에 섹터를 쓰는 동안 FS 드라이버가 같은 섹터를 건드려
//! 손상시키는 일을 막는다.
//!
//! 일반 경로는 `FSCTL_LOCK_VOLUME` / `FSCTL_DISMOUNT_VOLUME` / `FSCTL_UNLOCK_VOLUME`만
//! 사용한다. 잠긴 볼륨의 NTFS 부트 메타데이터 복구를 위해서만 crate 내부에 제한된 상대 오프셋
//! read/write를 제공한다. lock/dismount와 write 모두 알파 게이트 뒤에서만 접근한다.
#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;

use tracing::{info, instrument};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, SetFilePointerEx, WriteFile, FILE_BEGIN,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME, VOLUME_DISK_EXTENTS,
};
use windows::Win32::System::IO::DeviceIoControl;

use super::map_win_err;
use crate::{safety, ParqError, Result};

/// lock + dismount 된 볼륨 핸들. `Drop` 이 unlock + `CloseHandle` 을 보장한다(RAII).
///
/// crate 외부에는 write 기능을 노출하지 않는다. 내부 write는 lock+dismount 성공 후에만 가능하다.
pub struct VolumeLock {
    handle: HANDLE,
    letter: String,
    locked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeExtent {
    pub disk_number: u32,
    pub starting_offset_bytes: u64,
    pub extent_length_bytes: u64,
}

#[instrument]
pub fn query_volume_extent(drive_letter: &str) -> Result<VolumeExtent> {
    let letter = drive_letter.trim().trim_end_matches(':');
    let is_single_alpha = letter.len() == 1
        && letter
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic());
    if !is_single_alpha {
        return Err(ParqError::ValidationFailed(format!(
            "invalid drive letter: {drive_letter:?}"
        )));
    }

    let letter = letter.to_ascii_uppercase();
    let path = format!(r"\\.\{letter}:");
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE::default(),
        )
    }
    .map_err(|error| map_win_err(&format!("volume {letter}: open"), &error))?;

    let mut extents = VOLUME_DISK_EXTENTS::default();
    let mut returned = 0u32;
    let ioctl_result = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            None,
            0,
            Some(&mut extents as *mut _ as *mut c_void),
            std::mem::size_of::<VOLUME_DISK_EXTENTS>() as u32,
            Some(&mut returned),
            None,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    ioctl_result.map_err(|error| map_win_err("volume disk extents IOCTL", &error))?;

    if extents.NumberOfDiskExtents != 1 {
        return Err(ParqError::ValidationFailed(format!(
            "volume {letter}: has {} disk extents; exactly one is required",
            extents.NumberOfDiskExtents
        )));
    }
    let extent = extents.Extents[0];
    let starting_offset_bytes = u64::try_from(extent.StartingOffset).map_err(|_| {
        ParqError::ValidationFailed(format!("volume {letter}: has a negative starting offset"))
    })?;
    let extent_length_bytes = u64::try_from(extent.ExtentLength).map_err(|_| {
        ParqError::ValidationFailed(format!("volume {letter}: has a negative extent length"))
    })?;
    Ok(VolumeExtent {
        disk_number: extent.DiskNumber,
        starting_offset_bytes,
        extent_length_bytes,
    })
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

        // SAFETY: wide 는 NUL 종단 UTF-16. 반환 핸들은 즉시 VolumeLock(RAII)로 감싼다.
        //         crate 내부 write는 아래 lock+dismount 성공 뒤에만 허용한다.
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

    pub(crate) fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        if output.is_empty() {
            return Err(ParqError::ValidationFailed(
                "volume read buffer must not be empty".into(),
            ));
        }
        let offset: i64 = offset
            .try_into()
            .map_err(|_| ParqError::ValidationFailed("volume read offset exceeds i64".into()))?;
        unsafe { SetFilePointerEx(self.handle, offset, None, FILE_BEGIN) }
            .map_err(|error| map_win_err("volume SetFilePointerEx(read)", &error))?;
        let mut done = 0usize;
        while done < output.len() {
            let mut read = 0u32;
            unsafe {
                ReadFile(
                    self.handle,
                    Some(&mut output[done..]),
                    Some(&mut read),
                    None,
                )
            }
            .map_err(|error| map_win_err("volume ReadFile", &error))?;
            if read == 0 {
                return Err(ParqError::Platform(format!(
                    "volume read returned zero bytes: done={done}, total={}",
                    output.len()
                )));
            }
            done += read as usize;
        }
        Ok(())
    }

    pub(crate) fn write_all_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        if !self.locked {
            return Err(ParqError::ValidationFailed(
                "volume write requires a locked volume".into(),
            ));
        }
        if data.is_empty() {
            return Err(ParqError::ValidationFailed(
                "volume write buffer must not be empty".into(),
            ));
        }
        let offset: i64 = offset
            .try_into()
            .map_err(|_| ParqError::ValidationFailed("volume write offset exceeds i64".into()))?;
        unsafe { SetFilePointerEx(self.handle, offset, None, FILE_BEGIN) }
            .map_err(|error| map_win_err("volume SetFilePointerEx(write)", &error))?;
        let mut done = 0usize;
        while done < data.len() {
            let mut written = 0u32;
            unsafe { WriteFile(self.handle, Some(&data[done..]), Some(&mut written), None) }
                .map_err(|error| map_win_err("volume WriteFile", &error))?;
            if written == 0 {
                return Err(ParqError::Platform(format!(
                    "volume write returned zero bytes: done={done}, total={}",
                    data.len()
                )));
            }
            done += written as usize;
        }
        Ok(())
    }

    pub(crate) fn flush(&self) -> Result<()> {
        unsafe { FlushFileBuffers(self.handle) }
            .map_err(|error| map_win_err("volume FlushFileBuffers", &error))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::BusType;

    #[test]
    #[ignore = "requires an administrator-created disposable VHD volume"]
    fn locked_vhd_accepts_identical_boot_sector_roundtrip() {
        let letter = std::env::var("PARQ_TEST_VHD_DRIVE")
            .expect("PARQ_TEST_VHD_DRIVE must name a disposable VHD volume");
        let extent = query_volume_extent(&letter).expect("query test VHD extent");
        let layout = crate::disk::enumerate()
            .expect("enumerate test VHD")
            .into_iter()
            .find(|disk| disk.number == extent.disk_number)
            .expect("find test VHD");
        assert_eq!(layout.bus_type, BusType::Virtual);
        assert!(!layout.is_system);
        assert!(!layout.is_read_only);
        assert!(layout.size_bytes <= 128 * 1024 * 1024);

        let raw = crate::raw_io::open_physical_drive_readonly(extent.disk_number)
            .expect("open test VHD read-only");
        let sector_len = raw.geometry().logical_sector_bytes as usize;
        drop(raw);
        let volume = VolumeLock::lock_and_dismount(&letter).expect("lock test VHD volume");
        let mut before = vec![0u8; sector_len];
        volume
            .read_exact_at(0, &mut before)
            .expect("read test VHD boot sector");
        volume
            .write_all_at(0, &before)
            .expect("rewrite identical test VHD boot sector");
        volume.flush().expect("flush test VHD volume");
        let mut after = vec![0u8; sector_len];
        volume
            .read_exact_at(0, &mut after)
            .expect("verify test VHD boot sector");
        assert_eq!(after, before);
    }
}
