//! V2 raw 디스크 I/O — **read-only 파운데이션** (Phase 2 PR1–3).
//!
//! 근거: `docs/v2-raw-io.md`. 이 모듈은 `\\.\PhysicalDriveN` 을 **읽기 전용**으로 열어
//! geometry 조회 + 섹터 read 만 한다. **절대 write 하지 않는다** — `WriteFile` / FSCTL write
//! 호출이 이 파일에 등장하면 PR 거절. raw write / move 엔진은 별도 PR(charter §3-5, Phase 3).
//!
//! 안전 규칙 (docs/v2-raw-io.md §5):
//! - 모든 `unsafe` 블록에 `// SAFETY:` 주석.
//! - 핸들은 RAII(`RawDisk`)로 감싸 `Drop` 에서 `CloseHandle` 보장.
//! - 정렬: `FILE_FLAG_NO_BUFFERING` — 오프셋/길이/버퍼 주소 모두 섹터 배수(§3).
//!
//! Windows 전용. 비-Windows 빌드에는 이 모듈이 컴파일되지 않는다(lib.rs 의 `#[cfg(windows)]`).
#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;
use std::alloc::{alloc, dealloc, Layout};

use tracing::{debug, instrument};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, SetFilePointerEx, FILE_BEGIN, FILE_FLAG_NO_BUFFERING, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{DISK_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX};
use windows::Win32::System::IO::DeviceIoControl;

use crate::{ParqError, Result};

/// 버퍼 정렬 상한. 512e(512)·4Kn(4096) 모두 커버. `FILE_FLAG_NO_BUFFERING` 요구(§3).
const BUFFER_ALIGN: usize = 4096;

/// 스트리밍 read 청크 크기(바이트). 1 MiB(§3). 섹터 배수로 내림해서 사용.
const CHUNK_BYTES: u64 = 1024 * 1024;

/// 디스크 기하. `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` 결과에서 필요한 필드만.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskGeometry {
    /// 논리 섹터 크기(바이트). raw I/O 정렬의 기준 단위(§2.2).
    pub logical_sector_bytes: u32,
    /// 디스크 총 바이트 수.
    pub total_bytes: u64,
}

impl DiskGeometry {
    /// 총 논리 섹터 수.
    #[must_use]
    pub fn sector_count(&self) -> u64 {
        if self.logical_sector_bytes == 0 {
            0
        } else {
            self.total_bytes / self.logical_sector_bytes as u64
        }
    }
}

/// 섹터 정렬된 힙 버퍼. `NO_BUFFERING` read 대상.
///
/// `Vec<u8>` 은 u8 정렬(1)이라 `NO_BUFFERING` 요구(섹터/페이지 정렬)를 못 맞춘다. 그래서
/// `alloc` 으로 4096 정렬 메모리를 직접 확보한다.
struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
    layout: Layout,
}

impl AlignedBuf {
    fn new(len: usize) -> Result<Self> {
        debug_assert!(len > 0);
        let layout = Layout::from_size_align(len, BUFFER_ALIGN)
            .map_err(|e| ParqError::Platform(format!("정렬 버퍼 layout 실패: {e}")))?;
        // SAFETY: layout 의 size > 0 (호출자가 보장, chunk 는 최소 1 섹터). alloc 실패 시 null 체크.
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            return Err(ParqError::Platform(format!(
                "정렬 버퍼 할당 실패 ({len} bytes)"
            )));
        }
        Ok(Self { ptr, len, layout })
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr 은 new 에서 len 바이트로 유효하게 할당됨. Drop 전까지 유효. 공유 참조만 대여.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: 위와 동일. &mut self 로 배타적 대여 보장.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: ptr/layout 은 new 의 alloc 과 정확히 짝. 한 번만 dealloc.
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

/// 읽기 전용 raw 디스크 핸들 (RAII).
///
/// `open_physical_drive_readonly` 로만 생성. `Drop` 이 `CloseHandle` 을 호출해 핸들 누수를 막는다.
pub struct RawDisk {
    handle: HANDLE,
    number: u32,
    geometry: DiskGeometry,
}

impl RawDisk {
    #[must_use]
    pub fn number(&self) -> u32 {
        self.number
    }

    #[must_use]
    pub fn geometry(&self) -> DiskGeometry {
        self.geometry
    }
}

impl Drop for RawDisk {
    fn drop(&mut self) {
        // SAFETY: handle 은 CreateFileW 성공으로 얻은 유효 핸들. 한 번만 close (Drop 은 1회).
        //         반환 에러는 무시 — Drop 경로에서 panic 화하지 않는다.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// `\\.\PhysicalDriveN` 을 **읽기 전용**으로 열고 geometry 를 조회한다.
///
/// - `GENERIC_READ` 만 요청. write 비트 없음.
/// - `FILE_SHARE_READ | FILE_SHARE_WRITE` — 열거 도구와 공존(§2.1).
/// - `FILE_FLAG_NO_BUFFERING` — 정렬 규칙 강제(§3).
///
/// 관리자 권한 필요. 권한 없으면 `ERROR_ACCESS_DENIED` → `Platform` 에러.
#[instrument]
pub fn open_physical_drive_readonly(number: u32) -> Result<RawDisk> {
    let path = format!(r"\\.\PhysicalDrive{number}");
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: wide 는 NUL 종단 UTF-16. 인자는 docs/v2-raw-io.md §2.1 대로 read-only.
    //         반환 핸들은 아래에서 즉시 RawDisk(RAII)로 감싼다.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING,
            HANDLE::default(),
        )
    }
    .map_err(|e| map_win_err(&format!("PhysicalDrive{number} open"), &e))?;

    let mut disk = RawDisk {
        handle,
        number,
        geometry: DiskGeometry {
            logical_sector_bytes: 0,
            total_bytes: 0,
        },
    };
    disk.geometry = query_geometry(&disk)?;
    debug!(
        target: "parq::raw_io",
        number,
        logical_sector = disk.geometry.logical_sector_bytes,
        total_bytes = disk.geometry.total_bytes,
        "raw 디스크 읽기전용 open"
    );
    Ok(disk)
}

/// `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` 로 논리 섹터 크기 + 총 바이트 조회. read-only.
fn query_geometry(disk: &RawDisk) -> Result<DiskGeometry> {
    let mut geo = DISK_GEOMETRY_EX::default();
    let mut returned: u32 = 0;

    // SAFETY: out 버퍼는 DISK_GEOMETRY_EX 크기로 정확히 지정. read-only IOCTL(입력 버퍼 없음).
    unsafe {
        DeviceIoControl(
            disk.handle,
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            None,
            0,
            Some(&mut geo as *mut _ as *mut c_void),
            std::mem::size_of::<DISK_GEOMETRY_EX>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| map_win_err("geometry IOCTL", &e))?;

    let bps = geo.Geometry.BytesPerSector;
    if bps == 0 {
        return Err(ParqError::Platform(
            "디스크가 논리 섹터 크기 0 을 보고함".into(),
        ));
    }
    Ok(DiskGeometry {
        logical_sector_bytes: bps,
        total_bytes: geo.DiskSize as u64,
    })
}

/// `[lba, lba+count)` 섹터를 `buf` 로 읽는다. read-only.
///
/// 정렬(§3): 오프셋 = `lba * sector`, 길이 = `count * sector` — 둘 다 섹터 배수. `buf` 는
/// `AlignedBuf` 라 주소도 정렬됨. 부분 전송(§2.3) 대비 루프.
fn read_sectors(disk: &RawDisk, lba: u64, count: u32, buf: &mut AlignedBuf) -> Result<()> {
    let sector = disk.geometry.logical_sector_bytes as u64;
    let want = count as u64 * sector;

    if (buf.len as u64) < want {
        return Err(ParqError::ValidationFailed(format!(
            "버퍼({} bytes)가 요청({want} bytes)보다 작음",
            buf.len
        )));
    }
    // 디스크 범위 초과 방지.
    if lba.saturating_add(count as u64) > disk.geometry.sector_count() {
        return Err(ParqError::ValidationFailed(format!(
            "읽기 범위 초과: lba={lba} count={count} > sector_count={}",
            disk.geometry.sector_count()
        )));
    }

    let offset = (lba * sector) as i64;
    // SAFETY: 유효 핸들 + FILE_BEGIN 기준 절대 오프셋(섹터 배수). 새 포인터는 필요 없어 None.
    unsafe { SetFilePointerEx(disk.handle, offset, None, FILE_BEGIN) }
        .map_err(|e| map_win_err("SetFilePointerEx", &e))?;

    let want = want as usize;
    let mut done: usize = 0;
    while done < want {
        let mut read: u32 = 0;
        let slice = &mut buf.as_mut_slice()[done..want];
        // SAFETY: 유효 핸들. slice 는 정렬 버퍼의 섹터 배수 오프셋(done)부터 → 주소·길이 정렬 유지.
        unsafe { ReadFile(disk.handle, Some(slice), Some(&mut read), None) }
            .map_err(|e| map_win_err("ReadFile", &e))?;
        if read == 0 {
            return Err(ParqError::Platform(format!(
                "예상보다 이른 EOF (lba={lba}, done={done}, want={want})"
            )));
        }
        done += read as usize;
    }
    Ok(())
}

/// `[start_lba, start_lba+length_sectors)` 구간을 1 MiB 청크로 스트리밍하며 `f` 에 넘긴다.
///
/// 해시 계산 / 값 비교를 호출자가 소유하게 해서 이 모듈이 해시 의존성(sha2 등)을 갖지 않게 한다.
/// PR3 게이트(docs/v2-raw-io.md §7)의 SHA256 라운드트립은 호출자(example/test)가 `f` 에서 수행.
#[instrument(skip(disk, f), fields(number = disk.number))]
pub fn for_each_chunk<F>(
    disk: &RawDisk,
    start_lba: u64,
    length_sectors: u64,
    mut f: F,
) -> Result<()>
where
    F: FnMut(&[u8]) -> Result<()>,
{
    let sector = disk.geometry.logical_sector_bytes as u64;
    let chunk_sectors = (CHUNK_BYTES / sector).max(1);
    let mut buf = AlignedBuf::new((chunk_sectors * sector) as usize)?;

    let mut lba = start_lba;
    let mut remaining = length_sectors;
    while remaining > 0 {
        let n = remaining.min(chunk_sectors);
        read_sectors(disk, lba, n as u32, &mut buf)?;
        let byte_len = (n * sector) as usize;
        f(&buf.as_slice()[..byte_len])?;
        lba += n;
        remaining -= n;
    }
    Ok(())
}

/// windows 에러를 ParqError 로 매핑(docs/v2-raw-io.md §6).
fn map_win_err(ctx: &str, e: &windows::core::Error) -> ParqError {
    const E_ACCESS_DENIED: i32 = -0x7FF8_FFFB; // 0x80070005
    const E_SHARING_VIOLATION: i32 = -0x7FF8_FFE0; // 0x80070020
    let code = e.code().0;
    if code == E_ACCESS_DENIED {
        return ParqError::Platform(format!(
            "{ctx}: 접근 거부 — 관리자 권한 필요 또는 디스크 사용 중 ({e})"
        ));
    }
    if code == E_SHARING_VIOLATION {
        return ParqError::ValidationFailed(format!("{ctx}: 볼륨이 사용 중입니다 ({e})"));
    }
    ParqError::Platform(format!("{ctx}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 디스크 없이 검증 가능한 순수 로직만. raw 핸들 테스트는 VHD 통합 테스트(Phase 1 하네스) 소관.

    #[test]
    fn sector_count_divides_total() {
        let g = DiskGeometry {
            logical_sector_bytes: 512,
            total_bytes: 512 * 2048,
        };
        assert_eq!(g.sector_count(), 2048);
    }

    #[test]
    fn sector_count_zero_sector_is_zero_not_panic() {
        let g = DiskGeometry {
            logical_sector_bytes: 0,
            total_bytes: 12345,
        };
        assert_eq!(g.sector_count(), 0);
    }

    #[test]
    fn sector_count_4kn() {
        let g = DiskGeometry {
            logical_sector_bytes: 4096,
            total_bytes: 4096 * 100,
        };
        assert_eq!(g.sector_count(), 100);
    }

    #[test]
    fn aligned_buf_is_sector_aligned_and_usable() {
        let mut buf = AlignedBuf::new(4096).expect("alloc");
        assert_eq!(buf.ptr as usize % BUFFER_ALIGN, 0, "버퍼가 정렬되지 않음");
        // 쓰기/읽기 라운드트립으로 유효 메모리 확인.
        buf.as_mut_slice()[0] = 0xAB;
        buf.as_mut_slice()[4095] = 0xCD;
        assert_eq!(buf.as_slice()[0], 0xAB);
        assert_eq!(buf.as_slice()[4095], 0xCD);
    }
}
