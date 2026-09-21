//! raw 섹터 WRITE — **파괴적 프리미티브** (Phase 2 PR4). `docs/v2-raw-io.md §2.3·§3·§5`.
//!
//! ⚠ 이 파일의 코드는 디스크 섹터를 **덮어쓴다**. 잘못된 대상/오프셋은 데이터를 영구 파괴한다.
//! 그래서 write 핸들을 여는 유일한 경로(`open_writable`)는 두 겹의 가드 뒤에 있다:
//!
//!   1. `safety::require_v2_destructive()` — 알파 게이트(`PARQ_ENABLE_V2_DESTRUCTIVE`), charter §3-1.
//!   2. `safety::check_disk_writable()` — V1 디스크 가드 재사용: 시스템 디스크 / 읽기전용 / 외장
//!      아님(dev bypass 제외)을 차단. **승인이 있어도 시스템 디스크 raw write 는 코드가 거부한다.**
//!
//! charter §3-5 대로 write 코드는 read 코드와 분리된 이 파일에 격리한다. 실제 파티션 이동
//! (오프셋 계산, checkpoint, 인접 무변경 검증)은 이 프리미티브 위에 Phase 3 에서 얹는다.
#![deny(unsafe_op_in_unsafe_fn)]

use tracing::{info, instrument, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, SetFilePointerEx, WriteFile, FILE_BEGIN,
    FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::IOCTL_DISK_UPDATE_PROPERTIES;
use windows::Win32::System::IO::DeviceIoControl;

use super::{map_win_err, query_geometry, AlignedBuf, DiskGeometry};
use crate::{disk, safety, ParqError, Result};

/// 쓰기 가능 raw 디스크 핸들 (RAII). `open_writable` 로만 생성.
///
/// `Drop` 이 `CloseHandle` 을 호출한다. write 는 `write_sectors` 로만 — 정렬/범위 검증을 강제한다.
pub struct WritableDisk {
    handle: HANDLE,
    number: u32,
    geometry: DiskGeometry,
}

impl WritableDisk {
    #[must_use]
    pub fn number(&self) -> u32 {
        self.number
    }

    #[must_use]
    pub fn geometry(&self) -> DiskGeometry {
        self.geometry
    }

    /// `data`(섹터 배수 길이)를 `lba` 부터 쓴다. **파괴적.**
    ///
    /// 검증: 길이가 섹터 배수인가 / 범위가 디스크를 넘지 않는가. 정렬(§3): `data` 를 정렬 버퍼로
    /// 복사해 `NO_BUFFERING` 주소 정렬을 보장하고, 오프셋/길이는 섹터 배수. 완료 후 명시적 flush.
    #[instrument(skip(self, data), fields(number = self.number, lba, len = data.len()))]
    pub fn write_sectors(&self, lba: u64, data: &[u8]) -> Result<()> {
        let sector = self.geometry.logical_sector_bytes as usize;
        if sector == 0 {
            return Err(ParqError::Platform("논리 섹터 크기 0".into()));
        }
        if data.is_empty() || data.len() % sector != 0 {
            return Err(ParqError::ValidationFailed(format!(
                "write 길이({})가 섹터({sector}) 배수가 아님",
                data.len()
            )));
        }
        let count = (data.len() / sector) as u64;
        if lba.saturating_add(count) > self.geometry.sector_count() {
            return Err(ParqError::ValidationFailed(format!(
                "write 범위 초과: lba={lba} count={count} > sector_count={}",
                self.geometry.sector_count()
            )));
        }

        // NO_BUFFERING 정렬: 임의 &[u8] 주소는 정렬 보장이 없으므로 정렬 버퍼로 복사.
        let mut buf = AlignedBuf::new(data.len())?;
        buf.as_mut_slice().copy_from_slice(data);

        let offset = (lba * sector as u64) as i64;
        // SAFETY: 유효 핸들. FILE_BEGIN 기준 절대 오프셋(섹터 배수).
        unsafe { SetFilePointerEx(self.handle, offset, None, FILE_BEGIN) }
            .map_err(|e| map_win_err("SetFilePointerEx(write)", &e))?;

        let total = data.len();
        let mut done = 0usize;
        while done < total {
            let mut written: u32 = 0;
            let slice = &buf.as_slice()[done..total];
            // SAFETY: 유효 핸들. slice 는 정렬 버퍼의 섹터 배수 오프셋(done)부터 → 주소·길이 정렬 유지.
            unsafe { WriteFile(self.handle, Some(slice), Some(&mut written), None) }
                .map_err(|e| map_win_err("WriteFile", &e))?;
            if written == 0 {
                return Err(ParqError::Platform(format!(
                    "0 바이트 write (lba={lba}, done={done}, total={total})"
                )));
            }
            done += written as usize;
        }
        self.flush()?;
        Ok(())
    }

    /// `[lba, lba+out.len()/sector)` 섹터를 `out` 으로 읽는다. RW 핸들의 read 능력 사용.
    ///
    /// move 엔진이 src 영역을 읽을 때 쓴다(같은 핸들로 read+write → 핸들 하나로 이동). 정렬은
    /// 내부 정렬 버퍼로 보장. `out.len()` 은 섹터 배수여야 한다.
    pub fn read_sectors(&self, lba: u64, out: &mut [u8]) -> Result<()> {
        let sector = self.geometry.logical_sector_bytes as usize;
        if sector == 0 {
            return Err(ParqError::Platform("논리 섹터 크기 0".into()));
        }
        if out.is_empty() || out.len() % sector != 0 {
            return Err(ParqError::ValidationFailed(format!(
                "read 길이({})가 섹터({sector}) 배수가 아님",
                out.len()
            )));
        }
        let count = (out.len() / sector) as u64;
        if lba.saturating_add(count) > self.geometry.sector_count() {
            return Err(ParqError::ValidationFailed(format!(
                "read 범위 초과: lba={lba} count={count} > sector_count={}",
                self.geometry.sector_count()
            )));
        }

        let mut buf = AlignedBuf::new(out.len())?;
        let offset = (lba * sector as u64) as i64;
        // SAFETY: 유효 핸들, 섹터 배수 오프셋.
        unsafe { SetFilePointerEx(self.handle, offset, None, FILE_BEGIN) }
            .map_err(|e| map_win_err("SetFilePointerEx(read)", &e))?;

        let total = out.len();
        let mut done = 0usize;
        while done < total {
            let mut read: u32 = 0;
            let slice = &mut buf.as_mut_slice()[done..total];
            // SAFETY: 유효 핸들. slice 는 정렬 버퍼의 섹터 배수 오프셋부터 → 주소·길이 정렬 유지.
            unsafe { ReadFile(self.handle, Some(slice), Some(&mut read), None) }
                .map_err(|e| map_win_err("ReadFile", &e))?;
            if read == 0 {
                return Err(ParqError::Platform(format!(
                    "0 바이트 read (lba={lba}, done={done}, total={total})"
                )));
            }
            done += read as usize;
        }
        out.copy_from_slice(buf.as_slice());
        Ok(())
    }

    /// 캐시 flush. `WRITE_THROUGH` 를 이미 걸지만 checkpoint 경계 방어적 이중화(§2.4).
    pub fn flush(&self) -> Result<()> {
        // SAFETY: 유효 핸들.
        unsafe { FlushFileBuffers(self.handle) }.map_err(|e| map_win_err("FlushFileBuffers", &e))
    }

    /// 파티션 테이블을 바꾼 뒤 Windows 저장소 스택이 새 레이아웃을 다시 읽도록 요청한다.
    pub fn update_properties(&self) -> Result<()> {
        let mut returned = 0u32;
        // SAFETY: 유효한 디스크 핸들, 입력/출력 버퍼가 없는 IOCTL 호출.
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_DISK_UPDATE_PROPERTIES,
                None,
                0,
                None,
                0,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| map_win_err("IOCTL_DISK_UPDATE_PROPERTIES", &e))
    }
}

impl Drop for WritableDisk {
    fn drop(&mut self) {
        // SAFETY: CreateFileW 성공 핸들. Drop 1회 → 한 번만 close. 에러는 무시.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// `\\.\PhysicalDriveN` 을 **쓰기 가능**으로 연다. 두 겹 가드 뒤에서만 성공.
///
/// 1. 알파 게이트(`require_v2_destructive`) 2. 디스크 안전 가드(`check_disk_writable`).
/// 시스템 디스크 / 읽기전용은 여기서 거부된다. 관리자 권한 필요.
#[instrument]
pub fn open_writable(number: u32) -> Result<WritableDisk> {
    // (1) 알파 게이트
    safety::require_v2_destructive()?;

    // (2) V1 디스크 안전 가드 재사용 — 대상 디스크를 재열거해 검증한다.
    let disks = disk::enumerate()?;
    let target = disks
        .iter()
        .find(|d| d.number == number)
        .ok_or_else(|| ParqError::DiskNotFound(format!("PhysicalDrive{number}")))?;
    safety::check_disk_writable(target)?;

    warn!(
        target: "parq::raw_io",
        number,
        model = %target.model,
        "raw WRITABLE 핸들 open — 파괴적 경로 (알파 게이트 + 디스크 가드 통과)"
    );

    let path = format!(r"\\.\PhysicalDrive{number}");
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: wide 는 NUL 종단. RW + NO_BUFFERING + WRITE_THROUGH(§3). 반환 핸들은 즉시 RAII 로 감쌈.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
            HANDLE::default(),
        )
    }
    .map_err(|e| map_win_err(&format!("PhysicalDrive{number} writable open"), &e))?;

    let geometry = query_geometry(handle)?;
    let disk = WritableDisk {
        handle,
        number,
        geometry,
    };
    info!(
        target: "parq::raw_io",
        number,
        logical_sector = geometry.logical_sector_bytes,
        "raw WRITABLE 핸들 open 완료"
    );
    Ok(disk)
}
