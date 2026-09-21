# Parq Architecture (V0)

이 문서는 Parq 의 V0 ~ V1 구조를 설명한다. 안전 모델은 [safety-model.md](safety-model.md) 참고.

## 레이어

```
┌─────────────────────────────────────────────────────────────┐
│  Frontend (React + Vite + TS, src/)                         │
│  - 디스크 목록, 파티션 시각화, plan/preview UI              │
│  - 백엔드 호출은 @tauri-apps/api invoke() 만                │
└────────────────────────────┬────────────────────────────────┘
                             │ Tauri IPC (JSON)
┌────────────────────────────┴────────────────────────────────┐
│  Tauri commands (src-tauri/src/commands/)                   │
│  - read.rs : 조회 전용                                      │
│  - write.rs : 파괴적 작업 (V1 단계 추가)                    │
│  - 모든 write 는 safety guard 통과해야 실행                 │
└──────┬──────────────────┬───────────────────┬───────────────┘
       │                  │                   │
       ▼                  ▼                   ▼
┌─────────────┐ ┌──────────────────┐ ┌──────────────────┐
│ disk/       │ │ safety/          │ │ partition/, fs/  │
│ (read-only) │ │ - 시스템 보호    │ │ - 위험 영역      │
│             │ │ - 마운트 체크    │ │ - 4단계 패턴     │
│             │ │ - free space 등  │ │                  │
└──────┬──────┘ └────────┬─────────┘ └────────┬─────────┘
       │                 │                    │
       └─────────────────┼────────────────────┘
                         ▼
              ┌──────────────────────────┐
              │ transaction/             │
              │ - begin / commit / fail audit logging
              │ - %LOCALAPPDATA%\Parq\transactions\<uuid>.json
              └────────────┬─────────────┘
                           ▼
              ┌──────────────────────────┐
              │ platform/windows.rs      │
              │ - diskpart wrapper       │
              │ - PowerShell Storage 모듈│
              │ - WMI                    │
              │ (V2 이후 직접 IOCTL)     │
              └──────────────────────────┘
```

## 데이터 흐름: 파괴적 작업

예시: 사용자가 USB 디스크의 파티션을 포맷하려는 경우.

```
[사용자]                  [Frontend]              [Backend]
   │                          │                       │
   │  포맷 버튼 클릭           │                       │
   ├─────────────────────────▶│                       │
   │                          │  invoke('plan_format')│
   │                          ├──────────────────────▶│
   │                          │                       │ 1. plan: read-only 계산
   │                          │                       │ 2. validate: safety 가드
   │                          │   PartitionPlan       │
   │                          │◀──────────────────────┤
   │  preview 다이얼로그       │                       │
   │◀─────────────────────────┤                       │
   │  디스크 시리얼 입력 확인  │                       │
   ├─────────────────────────▶│                       │
   │                          │ invoke('execute_format│
   │                          │           _dangerous')│
   │                          ├──────────────────────▶│
   │                          │                       │ 3. transaction::begin
   │                          │                       │ 4. platform::format
   │                          │                       │ 5. transaction::commit
   │                          │   ExecutionResult     │
   │                          │◀──────────────────────┤
   │  결과 표시               │                       │
   │◀─────────────────────────┤                       │
```

`plan` 단계와 `execute_*_dangerous` 단계가 **분리된 IPC** 라는 점이 핵심. 사용자가 preview 를 본 뒤 명시적으로 두 번째 호출을 트리거해야 실행된다.

## V1 백엔드 전략: 시스템 툴 래핑

V1 에서는 직접 IOCTL 호출을 피하고 검증된 시스템 툴을 래핑한다:

| 작업              | V1 구현                             | V2 후보 (직접 IOCTL)             |
|-------------------|-------------------------------------|----------------------------------|
| 디스크 열거       | WMI (`Win32_DiskDrive`)             | `IOCTL_STORAGE_QUERY_PROPERTY`   |
| 파티션 열거       | WMI (`MSFT_Partition`)              | `IOCTL_DISK_GET_DRIVE_LAYOUT_EX` |
| 파티션 생성/삭제  | PowerShell `New/Remove-Partition`   | `IOCTL_DISK_SET_DRIVE_LAYOUT_EX` |
| 포맷              | PowerShell `Format-Volume`          | `FSCTL_*`                        |
| 라벨 변경         | PowerShell `Set-Volume`             | `SetVolumeLabelW`                |
| 리사이즈          | PowerShell `Resize-Partition`       | (V2 이후 검토)                   |
| MBR ↔ GPT 변환    | (V1 미지원)                          | (V2 이후)                        |

이유:
1. **검증된 코드**: 마이크로소프트가 유지보수, 버그가 우리 책임 아님.
2. **재현 가능**: 사용자가 같은 명령을 직접 검증할 수 있음.
3. **속도**: V0 빠르게 출시.
4. **위험 분산**: 우리 코드의 IOCTL 버그가 데이터를 파괴할 가능성 차단.

단점:
- 외부 프로세스 호출 오버헤드.
- 출력 파싱 의존성 (PowerShell 은 JSON 출력 가능 — `ConvertTo-Json` 활용).
- 일부 작업은 시스템 툴이 지원 안 함.

## 모듈 책임

| 모듈              | 책임                                              | 쓰기 가능? |
|-------------------|---------------------------------------------------|------------|
| `commands/`       | Tauri IPC 진입점, 입력 검증                       | (위임만)   |
| `disk/`           | 디스크/파티션 조회                                | ❌         |
| `partition/`      | 파티션 plan/execute (4단계)                       | ✅ via txn |
| `fs/`             | 파일시스템 plan/execute (4단계)                   | ✅ via txn |
| `safety/`         | 시스템 디스크/볼륨 보호 검증                      | ❌         |
| `transaction/`    | 작업 로그, 커밋, 롤백                             | ✅ (로그만)|
| `platform/`       | OS 어댑터 (Windows: diskpart/PS/WMI 래퍼)         | ✅ via txn |
| `error.rs`        | 통일된 `ParqError` enum                           | ❌         |

## 비-결정사항 (TBD)

- 다국어 (i18n): V1 한국어/영어. 라이브러리 미정 (react-i18next vs 자체 함수)
- 상태 관리: V0 mock data 단계는 prop drilling. V1 실데이터 들어오면 zustand 또는 react-query 검토.
- 차트 라이브러리: 파티션 막대는 자체 구현 (현재). 향후 디스크 사용량 도넛 등 필요 시 검토.

## 변경 이력

- 2026-04-29: V0 초기 스케치
