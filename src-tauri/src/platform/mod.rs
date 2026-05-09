// Windows 전용 어댑터.
//
// V1 에서는 직접 IOCTL 호출 대신 검증된 시스템 툴 (PowerShell Storage 모듈, WMI) 을 래핑한다.
// 직접 IOCTL 은 V2 이후 충분한 테스트 인프라가 갖춰진 뒤에.
//
// - `wmi`        : MSFT_Disk / MSFT_Partition / MSFT_Volume read-only 열거
// - `powershell` : 파괴적 작업용 PowerShell 명령 실행 래퍼

#[cfg(windows)]
pub mod powershell;

#[cfg(windows)]
pub mod wmi;
