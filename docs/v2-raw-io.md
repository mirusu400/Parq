# Parq V2 — Raw I/O 인벤토리

> Phase 0 설계 문서 (2/5). **코드 0줄.** 이 문서는 V2 가 사용할 Windows raw I/O 원시(primitive)
> 들을 열거하고, 각각을 `windows-rs` API 에 매핑하고, 안전/정렬/캐시 규칙을 못박는다.
> `v2-charter.md` §2·§3·§6 을 전제로 한다.

**상태**: Draft — 리뷰 대기
**작성일**: 2026-07-09
**선행**: `v2-charter.md` (Ratified)
**후행 의존**: `v2-checkpoint-format.md`, `v2-move-algorithm.md` 는 이 문서의 원시들을 인용한다.

---

## 0. 이 문서의 범위

V1 은 raw 디스크 핸들을 **한 번도 열지 않았다** — 전부 PowerShell `Storage` 모듈 래핑이었다.
V2 는 "시작 LBA 이동"을 위해 `\\.\PhysicalDriveN` 을 직접 열고 섹터를 읽고(먼저), 나중에 쓴다.
이 문서는 **무엇을 호출하는지**와 **각 호출의 안전 전제**를 정의한다. **어떻게 이동하는지**(알고리즘)
는 `v2-move-algorithm.md`, **어떻게 복구하는지**는 `v2-checkpoint-format.md` 소관이다.

charter §3-5 규칙 재확인: **raw I/O read 코드와 write 코드는 반드시 별도 PR.** 이 문서는 둘 다
명세하지만, 구현 PR 은 read(PR1–PR3) → 검증 → write(PR4) 순서를 강제한다.

---

## 1. 핸들 인벤토리

V2 가 여는 커널 오브젝트는 3종뿐이다. 그 외 경로는 열지 않는다.

| # | 경로 | 용도 | 접근 | V2 단계 |
|---|------|------|------|---------|
| H1 | `\\.\PhysicalDriveN` | 디스크 전체 raw read/write, geometry | read→R/W | Phase 2–3 |
| H2 | `\\.\X:` (볼륨) | 볼륨 lock / dismount / flush | R/W (lock 용) | Phase 2 |
| H3 | (없음) | 파일시스템 파일 핸들은 열지 않는다 | — | — |

`N` 은 `Disk::number` (기존 `disk/mod.rs`), `X:` 는 `Partition::drive_letter`. 둘 다 **런타임에
재열거**해서 얻는다 — 절대 하드코딩 금지 (CLAUDE.md 절대 금지 규칙).

> **주의**: `Disk::number` 는 재부팅/재연결마다 바뀔 수 있어 V1 은 `Disk::id`(시리얼/GUID)를
> 안정 식별자로 쓴다. raw open 직전에 `disk::enumerate()` 로 `id → number` 를 **재확인**한 뒤
> 그 즉시 핸들을 연다. id 불일치 시 즉시 중단.

---

## 2. windows-rs API 매핑

의존성: `windows` crate (charter §6 승인됨). 필요한 feature:

```toml
# 이미 Cargo.toml 에 있는 것
"Win32_Foundation",
"Win32_Storage_FileSystem",   # CreateFileW, ReadFile, WriteFile, FlushFileBuffers
"Win32_System_IO",            # DeviceIoControl, OVERLAPPED
"Win32_System_Ioctl",         # IOCTL_DISK_*, FSCTL_* 상수, 구조체
# V2 에서 추가 검토
"Win32_System_Threading",     # (필요 시) 이벤트 기반 OVERLAPPED
```

### 2.1 핸들 열기 — `CreateFileW`

```
windows::Win32::Storage::FileSystem::CreateFileW
```

| 인자 | raw 디스크 값 | 근거 |
|------|--------------|------|
| `lpFileName` | `\\.\PhysicalDriveN` (UTF-16, NUL 종단) | Win32 디바이스 네임스페이스 |
| `dwDesiredAccess` | read: `GENERIC_READ` / write: `GENERIC_READ \| GENERIC_WRITE` | 최소 권한. read PR 에선 WRITE 비트 없음 |
| `dwShareMode` | `FILE_SHARE_READ \| FILE_SHARE_WRITE` | 열거 도구와 공존. 단, write 전 볼륨 lock 필수(§4) |
| `lpSecurityAttributes` | `None` | |
| `dwCreationDisposition` | `OPEN_EXISTING` | 디바이스는 생성 대상이 아님 |
| `dwFlagsAndAttributes` | `FILE_FLAG_NO_BUFFERING \| FILE_FLAG_WRITE_THROUGH` | §3 정렬·캐시 규칙 |
| `hTemplateFile` | `None` | |

반환 `HANDLE` 은 **`INVALID_HANDLE_VALUE` 체크 필수** → 실패 시 `GetLastError` 를
`ParqError::Platform` 으로 변환. 관리자 권한 없으면 여기서 `ERROR_ACCESS_DENIED(5)`.

### 2.2 geometry / 길이 조회 (read-only, `DeviceIoControl`)

```
windows::Win32::System::IO::DeviceIoControl
```

| IOCTL | 반환 구조체 | 얻는 값 |
|-------|-------------|---------|
| `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` | `DISK_GEOMETRY_EX` | 총 바이트 수, `DISK_GEOMETRY.BytesPerSector` |
| `IOCTL_STORAGE_QUERY_PROPERTY` (`StorageAccessAlignmentProperty`) | `STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR` | `BytesPerLogicalSector`, `BytesPerPhysicalSector` |
| `IOCTL_DISK_GET_PARTITION_INFO_EX` | `PARTITION_INFORMATION_EX` | 파티션 시작 offset / 길이 (교차 검증용) |

**논리 섹터 크기(`BytesPerLogicalSector`)가 raw I/O 정렬의 기준 단위.** 512e 디스크는 512,
4Kn 디스크는 4096. VHD 는 보통 512. V2 는 두 경우 모두 지원해야 한다 (`v2-test-infrastructure.md`
매트릭스에 4Kn 포함).

### 2.3 read / write — `ReadFile` / `WriteFile`

```
windows::Win32::Storage::FileSystem::{ReadFile, WriteFile}
```

- 버퍼 크기·파일 오프셋·버퍼 주소 **모두 논리 섹터의 배수**여야 한다 (§3, `FILE_FLAG_NO_BUFFERING`
  강제 조건). 위반 시 `ERROR_INVALID_PARAMETER(87)`.
- 오프셋 지정: `OVERLAPPED.Offset` / `OffsetHigh` (64-bit) 로 seek. `SetFilePointerEx` 대신
  OVERLAPPED 을 쓰는 게 sync 호출에서도 명확.
- 부분 전송 가능 → 반환된 `bytesTransferred` 를 항상 확인하고 루프.

### 2.4 캐시 flush — `FlushFileBuffers`

```
windows::Win32::Storage::FileSystem::FlushFileBuffers
```

`FILE_FLAG_WRITE_THROUGH` 를 이미 걸지만, checkpoint 경계에서 **명시적 flush 를 추가로 호출**한다
(방어적 이중화). checkpoint fsync 순서는 `v2-checkpoint-format.md` 가 규정.

---

## 3. 정렬 / 캐시 규칙 (violation = 즉시 실패)

`FILE_FLAG_NO_BUFFERING` 을 쓰는 이유: OS 페이지 캐시를 우회해서 **"쓴 것이 실제로 매체에
갔다"** 를 통제하려는 것. 대가로 3중 정렬 제약이 붙는다.

1. **파일 오프셋** = `k × BytesPerLogicalSector`
2. **전송 바이트 수** = `m × BytesPerLogicalSector`
3. **버퍼 메모리 주소** = 논리 섹터 크기(또는 페이지 크기)로 정렬

규칙:
- V2 는 **모든 I/O 를 섹터 단위로만** 수행한다. 바이트 단위 API 를 상위로 노출하지 않는다.
- 버퍼는 페이지 정렬 할당(`VirtualAlloc` 또는 정렬 보장 Rust 할당)로 확보. 임의 `Vec<u8>` 슬라이스
  주소를 그대로 넘기지 않는다.
- 청크 크기 기본 **1 MiB** (512-섹터 = 2048개, 4Kn = 256개). checkpoint 간격과 연동(`checkpoint-format`).
- `FILE_FLAG_WRITE_THROUGH` + checkpoint 마다 `FlushFileBuffers` = "이 지점 이전 데이터는 매체
  도달 보장". 이 보장이 kill-test 복구의 전제.

---

## 4. 볼륨 lock / dismount 프로토콜

디스크에 raw write 하는 동안 파일시스템 드라이버가 같은 섹터를 건드리면 손상된다. 그래서
**write 전 반드시 대상 볼륨을 lock + dismount** 한다.

| FSCTL | 대상 핸들 | 의미 |
|-------|-----------|------|
| `FSCTL_LOCK_VOLUME` | H2 (`\\.\X:`) | 다른 핸들 없을 때만 성공. 성공 시 독점 |
| `FSCTL_DISMOUNT_VOLUME` | H2 | 파일시스템 드라이버를 볼륨에서 분리 |
| `FSCTL_UNLOCK_VOLUME` | H2 | 작업 종료 후 해제 (또는 핸들 close 로 자동 해제) |

프로토콜 (write 작업당):
```
1. 대상 파티션의 볼륨 핸들 H2 open (\\.\X:)
2. FSCTL_LOCK_VOLUME  → 실패하면(다른 열린 핸들 존재) 작업 전체 중단, 사용자에게 "볼륨 사용 중" 안내
3. FSCTL_DISMOUNT_VOLUME
4. --- 이 구간에서만 PhysicalDrive raw write 허용 ---
5. 작업 완료 / 실패 무관하게 FSCTL_UNLOCK_VOLUME + H2 close
```

- **이동은 인접 파티션 데이터를 옮기므로, 이동 대상 볼륨뿐 아니라 겹치는 영역을 가진 모든 볼륨을
  lock 해야 한다** (보통은 이동 대상 1개). lock 실패한 볼륨이 하나라도 있으면 전체 중단.
- lock 을 못 잡으면 **절대 강제하지 않는다.** dismount 강제(force) 플래그류는 charter 비목표(§2).
- V1 의 safety 가드(`check_partition_destructive` = 마운트 상태 거부)와 상호보완: V2 는 "드라이브
  문자 없음"뿐 아니라 커널 레벨 lock 까지 요구한다.

---

## 5. `unsafe` 격리 정책 (charter §6)

raw I/O 는 불가피하게 `unsafe`. 규칙:

- 모든 `unsafe` 블록에 `// SAFETY:` 주석 (무엇이 왜 안전한지).
- `#[deny(unsafe_op_in_unsafe_fn)]` 을 raw I/O 모듈에 적용.
- `unsafe` 는 **얇은 FFI 래퍼 함수**에만. 각 래퍼는 안전한 시그니처를 export:
  ```
  // 예시 시그니처 (구현 아님 — Phase 2)
  fn open_physical_drive_readonly(number: u32) -> Result<RawDisk>;
  fn read_sectors(disk: &RawDisk, lba: u64, count: u32, buf: &mut AlignedBuf) -> Result<()>;
  fn geometry(disk: &RawDisk) -> Result<DiskGeometry>;      // logical/physical sector, total bytes
  ```
- 핸들은 RAII 래퍼(`RawDisk`, `VolumeLock`)로 감싸 `Drop` 에서 `CloseHandle` / `UNLOCK` 보장.
- write 함수(`write_sectors`)는 별도 PR(PR4), 그리고 `safety::v2_enabled()` 게이트 뒤에서만 도달
  가능하게 배선.

---

## 6. 에러 매핑

| Win32 오류 | 상황 | ParqError |
|-----------|------|-----------|
| `ERROR_ACCESS_DENIED(5)` | 관리자 권한 없음 / 핸들 경합 | `Platform("관리자 권한 필요 또는 디스크 사용 중")` |
| `ERROR_INVALID_PARAMETER(87)` | 정렬 위반 | `Platform` (개발 버그 — 정렬 로직 결함) |
| `ERROR_SHARING_VIOLATION(32)` | lock 실패 | `ValidationFailed("볼륨이 사용 중입니다")` |
| `ERROR_WRITE_PROTECT(19)` | 읽기 전용 매체 | `ValidationFailed` |
| geometry IOCTL 실패 | 지원 안 되는 디바이스 | `Platform` |

기존 `ParqError`(error.rs)에 새 variant 를 추가할지 여부는 PR 리뷰에서 결정 — 현 시점 판단으론
`Platform` / `ValidationFailed` 재사용으로 충분.

---

## 7. read-only 검증 게이트 (PR3 완료 조건)

write 코드(PR4)에 착수하려면 read 파운데이션이 다음을 통과해야 한다:

- [x] `\\.\PhysicalDriveN` 로 열어 geometry 조회 → 512 / 10 GiB, `Get-Disk` 값과 일치. *(2026-07-09, VM Disk#1 실기 검증. VHD 4Kn 은 아래 참고)*
- [x] 임의 LBA 범위 read → **SHA256 라운드트립 일치**: Rust(raw_read) 와 `hash-region.ps1` 이 VM Disk#1 LBA0..2048 에서 `56ba9084…16ea53` 동일. 시그니처(LBA0 보호MBR, LBA1 "EFI PART")도 정위치. *(2026-07-09, PR3 게이트 통과. sha2 는 dev-dependency)*
- [ ] 4Kn VHD 와 512e VHD 양쪽에서 정렬 규칙 준수 확인 (오프셋/길이 섹터 배수). *(512e 검증됨. 4Kn 은 vhd-matrix.ps1 로 픽스처 만든 뒤)*
- [x] 권한 없음 → 깔끔한 `ParqError`(접근거부→`Platform`), 패닉 없음. *(2026-07-09, 비관리자 셸에서 확인)*
- [x] 핸들 RAII: `RawDisk`/`AlignedBuf` 의 `Drop` 에서 `CloseHandle`/`dealloc`. *(코드 검증. 누수 도구 확인은 후속)*

이 5개 통과 전에는 `WriteFile` 을 부르는 코드를 **작성하지 않는다.**

---

## 8. 미결정 / 후속 논의

- **버퍼 할당 방식**: `VirtualAlloc` vs 정렬 보장 Rust allocator. Phase 2 PR1 에서 결정.
- **동기 vs 비동기 I/O**: V2 는 sync + 별도 스레드(진행률 콜백)로 시작. 완전 async(OVERLAPPED
  이벤트)는 필요성 확인 후.
- **4Kn 실기기 테스트**: 대부분 VHD 는 512. 4Kn 은 `Set-VHD -PhysicalSectorSizeBytes 4096` 로
  생성 가능한지 `v2-test-infrastructure.md` 에서 확정.

---

*raw I/O 는 Parq 가 커널과 직접 대화하는 지점이다. 여기서의 실수 하나가 사용자 디스크를 벽돌로
만든다. 그래서 이 문서의 규칙은 권고가 아니라 게이트다.*
