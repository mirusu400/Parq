# Parq Safety Model

> *"사용자가 우리를 신뢰해서 디스크를 맡겼다. 그 신뢰를 깨면 끝이다."*

이 문서는 Parq가 데이터 손실을 어떻게 방지하는지 정의한다. 새 기능을 추가하려면 먼저 이 문서를 읽고, 새로운 위험을 도입한다면 이 문서를 갱신한 PR을 함께 올린다.

## 위협 모델

Parq가 막아야 하는 시나리오:

1. **잘못된 디스크 선택** — 사용자가 USB로 알았는데 실제로는 시스템 디스크
2. **사용 중인 볼륨 조작** — 마운트되어 파일이 열려 있는 볼륨에 파괴적 작업
3. **부팅 의존 영역 파괴** — BitLocker 키, 페이지파일, 하이버네이션 파일을 가진 볼륨
4. **작업 중간 크래시 / 정전** — 파티션 테이블이 inconsistent 상태로 남음
5. **소프트웨어 버그 / 잘못된 IOCTL** — 우리 코드가 잘못된 LBA에 쓰는 경우
6. **Race condition** — 다른 도구가 동시에 디스크를 변경하는 경우

## 4단계 작업 패턴

모든 파괴적 작업(파티션 생성/삭제/포맷/리사이즈)은 다음 4단계를 **순서대로** 거친다. 단계를 건너뛰는 코드는 PR 거절 대상.

### 1. Plan (read-only)

```rust
let plan: PartitionPlan = partition::plan_<operation>(&disk, params)?;
```

- 디스크 상태를 읽고, 어떤 변경이 필요한지 **계산만** 한다.
- 디스크에 어떤 쓰기도 하지 않는다.
- 결과: `PartitionPlan` 구조체. 사용자에게 보여줄 수 있는 형태.

### 2. Validate (safety guards)

```rust
safety::validate(&plan)?;
```

다음을 모두 통과해야 한다:

- 대상이 시스템 디스크가 아니다. 단, 현재 부팅 중인 NTFS 볼륨의 `Resize-Partition` 온라인
  리사이즈만 전용 가드로 예외 허용한다.
- 대상 BitLocker 상태가 `NotEncrypted` 로 확인된다. 암호화/잠김/조회 실패는 모두 거부한다.
- 삭제/리사이즈/이동 대상은 마운트되어 있지 않다 (V1은 드라이브 문자 존재를 사용 중 신호로 본다).
- 충분한 free space (리사이즈/이동 시)
- 외장/제거 가능 미디어인지 확인 (V1 화이트리스트)
- execute 직전 디스크를 재열거하고 동일 입력으로 plan을 다시 계산했을 때 preview plan과 완전히 같다.

시스템 볼륨 리사이즈 예외는 `is_system disk + is_boot partition + NTFS + drive letter +
BitLocker NotEncrypted + writable disk` 조건을 모두 요구한다. EFI/MSR/Recovery와 시스템 볼륨
이동은 이 예외에 포함되지 않는다.

시스템 볼륨의 시작 LBA 이동은 별도의 개발자용 WinPE 경로만 허용한다. 실제 WinPE 환경
(`SystemDrive=X:`, `wpeutil.exe`, `MiniNT` 레지스트리)을 모두 확인하고
`PARQ_ENABLE_V2_DESTRUCTIVE=1`과 `PARQ_ENABLE_OFFLINE_SYSTEM_MOVE=1`이 함께 설정되어야 한다.
또한 GPT/NTFS, BitLocker 완전 해제, 대상 및 체크포인트 디스크의 크기·모델·시리얼,
원본/대상 LBA, 체크포인트 볼륨 extent, 두 디스크의 물리적 분리, 강한 확인 문구를 실행 직전에
다시 검증한다. 디스크 번호가 WinPE 부팅 후 달라지면 자동 추정하지 않고 중단한다. 자세한 운용
절차는 `winpe-offline-system-move.md`를 따른다.

검증 실패 시 `ParqError::SystemPartitionProtected`, `ValidationFailed` 등으로 거부. 우회 플래그(`--force`)는 V1에 추가하지 않는다.

### 3. Preview (사용자 명시적 확인)

- Tauri command가 `PartitionPlan`을 프론트엔드에 반환.
- 프론트엔드는 다음을 사람이 읽을 수 있는 형태로 보여준다:
  - 어떤 디스크인지 (모델명, 시리얼, 크기)
  - 어떤 파티션이 변경/삭제/생성되는지
  - 데이터 손실이 발생하는 영역 (강조)
  - 예상 소요 시간
- 사용자는 디스크 시리얼/라벨을 **타이핑**해서 확인한다 (단순 OK 버튼 금지).
- 빨간색은 **돌이킬 수 없는** 작업에만. 위험하지만 되돌릴 수 있는 작업은 노란색/주황색.

### 4. Execute (트랜잭션 + 실행)

```rust
let txn = transaction::begin(&plan)?;  // 디스크에 쓰기 전 로그 기록
match partition::execute(&plan, &txn) {
    Ok(_) => txn.commit()?,
    Err(e) => {
        txn.fail()?;  // 실패 사실을 기록; 자동 원상복구를 의미하지 않음
        return Err(e);
    }
}
```

- 트랜잭션 감사 로그는 `%LOCALAPPDATA%\Parq\transactions\<id>.json` 에 fsync로 기록한다.
- V1의 `failed` 결과는 실패 사실을 뜻하며 자동 원상복구를 뜻하지 않는다. 생성 중 포맷 실패처럼
  부분 성공 가능성이 있는 작업은 단계 로그를 보고 수동 확인해야 한다.
- V2 MBR/GPT 이동은 별도 checkpoint와 SHA256 검증으로 중단 후 재개한다. GPT는 backup
  엔트리·헤더를 먼저 기록하고 primary 엔트리·헤더를 기록하며 각 경계를 checkpoint에 남긴다.

## 시스템 디스크 정의

`safety::is_system_disk()`는 다음 중 **하나라도** 해당되면 `true`:

- Windows 부팅 볼륨이 위치한 디스크 (`GetSystemDirectoryW` → 볼륨 → 디스크)
- EFI 시스템 파티션이 있는 디스크
- 페이지파일이 위치한 볼륨이 있는 디스크 (`Win32_PageFileUsage`)
- 하이버네이션 파일(`hiberfil.sys`)이 있는 볼륨이 있는 디스크
- 현재 사용자 프로필이 있는 볼륨이 있는 디스크

**V1에서 시스템 디스크 작업은 차단**한다. UI에서도 read-only로만 표시.

## 외장 미디어 화이트리스트 (V1)

V1에서 쓰기 작업을 허용하는 디스크는 다음을 모두 만족해야 한다:

- `BusType`이 `USB`, `SD`, `MMC`, `IEEE1394`, 또는 `Storage Spaces`(외장)
- 또는 `IsRemovable == true`
- 그리고 시스템 디스크 정의의 어떤 조건에도 해당되지 않음

내부 SATA/NVMe SSD/HDD는 V1에서 read-only. V2에서 확장.

### 개발 전용 우회 (`PARQ_DEV_ALLOW_INTERNAL_DISKS`)

VM 환경 (VMware/Hyper-V) 의 가상 NVMe 디스크나 `Mount-VHD` 로 마운트한 VHDX 는
`BusType=Virtual` 또는 `BusType=NVMe` 로 인식되어 위 화이트리스트에 막힌다.
개발자가 이런 환경에서 destructive 작업을 테스트할 수 있도록 환경 변수를 통한 명시적
우회를 둔다.

```sh
# 개발/테스트용 — 일반 사용자 환경에서는 절대 설정하지 말 것
PARQ_DEV_ALLOW_INTERNAL_DISKS=1 cargo run --example enumerate
PARQ_DEV_ALLOW_INTERNAL_DISKS=1 cargo tauri dev
```

규칙:

- 우회되는 가드는 **bus-type 검사뿐** 이다. 시스템 디스크 / 읽기 전용 / 부팅 파티션 /
  시스템 파티션 / 마운트 상태 가드는 모두 그대로 적용된다.
- 우회 활성 시 `WARN parq::safety` 로그가 매 호출마다 기록되어 사용 흔적을 남긴다.
- UI / 사용자 설정 / `--force` CLI 플래그 / `tauri.conf` 어디에도 노출하지 않는다.
  유일한 활성화 경로는 환경 변수.
- 빌드 모드(debug/release) 와 무관하게 동작 — 릴리즈 빌드를 가져가서 실수로 켤 수도 있는
  대신, 환경 변수 명을 의도적으로 길고 명시적으로 잡았다.

## 트랜잭션 로그 포맷

```json
{
  "id": "uuid-v4",
  "started_at": "ISO-8601",
  "operation": "delete_partition",
  "disk": {
    "id": "disk-serial-or-id",
    "model": "...",
    "size_bytes": 0
  },
  "plan_hash": "sha256",
  "before": { "...디스크 상태 스냅샷..." },
  "steps": [
    { "step": "...", "status": "pending|done|failed" }
  ],
  "ended_at": null,
  "result": "committed | failed: <reason> | dropped_without_finalize | null"
}
```

크래시 후 로그는 UI에서 확인할 수 있다. V2 이동은 동일 이동 요청 시 checkpoint에서 재개하며,
시작 시 자동 복구 안내 UI는 아직 후속 과제다.

## 절대 금지 사항 (코드 레벨)

다음은 코드에 **절대로** 들어가면 안 된다:

- `unwrap()` / `expect()` 프로덕션 경로
- 안전 가드 우회 (`--force`, `unsafe_skip_check` 등)
- 트랜잭션 없이 디스크에 쓰는 코드
- `\\.\PhysicalDriveN` 하드코딩
- 자동화된 테스트가 실제 물리 디스크에 쓰기
- 시스템 디스크 검사를 디스크 인덱스(0번이면 시스템 등)로 추정 — **반드시** 위 정의된 검사 사용

## 테스트 환경

- 모든 파괴적 작업 테스트는 VHD/VHDX에서. (`scripts/create-test-vhd.ps1`)
- 통합 테스트는 단일 스레드 (`--test-threads=1`)
- CI에서 admin 권한 쓰기 테스트 금지

## 변경 이력

이 문서는 안전 모델을 변경하는 모든 PR에서 함께 갱신한다. 갱신 없이 모델을 우회하는 코드는 머지 거부.

- 2026-04-28: 초기 작성 (V0 스캐폴드)
- 2026-09-29: 개발자용 WinPE 시스템 볼륨 이동 게이트와 실행 전 fingerprint 검증 명시
