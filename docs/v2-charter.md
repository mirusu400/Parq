# Parq V2 Charter

> 이 문서는 V2 진입의 **헌장** 이다. V2 코드를 한 줄 작성하기 전에 이 문서의 전제들이
> 모두 합의되어야 한다. 합의 후 변경은 PR 로만, 본인 + 1 reviewer 승인 후.

**상태**: Ratified (9절 미결정 항목 모두 sign-off 완료, 2026-05-09)

**작성일**: 2026-05-09

---

## 1. 배경

V1 은 **시스템 툴 래핑 + plan/execute 분리** 로 안전한 파괴적 작업의 토대를 만들었다:

- USB / SD / 외장 디스크 한정
- PowerShell `Storage` 모듈 (`New-Partition`, `Resize-Partition`, `Remove-Partition`, ...) 만 사용
- 모든 작업이 4단계 (plan → validate → preview → execute) + 트랜잭션 로그
- raw 디스크 핸들 / IOCTL / 파일시스템 메타데이터 직접 쓰기 **0건**

V1 의 한계가 V2 의 동기다:

- **시작 LBA 이동 불가** — Windows API (`Resize-Partition`, `diskpart`) 가 끝 경계만 움직임
- **무손실 ↔ 파괴적 경계의 모호함** — V1 의 NTFS 리사이즈는 무손실이지만, 파일시스템 종류가 늘면 경계가 흐려짐
- **시스템 디스크 작업 불가** — 부팅된 디스크는 V1 에서 항상 거부

V2 는 이 한계 중 일부를 **시스템 툴 래핑이 아닌 직접 구현** 으로 푼다. 이 전환은 데이터 안전 위험을
한 단계 끌어올린다 — 따라서 V2 는 V1 보다 **더 보수적인 게이팅, 더 많은 테스트 인프라, 더 엄격한
복구 보장** 을 요구한다.

---

## 2. V2 스코프

### V2 에 들어간다 (IN)

1. **파티션 이동 (move)** — 시작 LBA 변경 가능. 디스크 내 임의 미할당 영역으로.
2. **raw 디스크 I/O 파운데이션** — `\\.\PhysicalDriveN` open / read / write, sector geometry.
3. **이동 기반 고급 리사이즈** — shrink-then-move, move-then-extend 같은 합성 작업.
4. **무손실 FS 리사이즈 확장 후보** — FAT32 / exFAT 의 안전한 일부 케이스 (조사 후 판단).
5. **kill-test 가능한 checkpoint 포맷** — 진행 중 전원 차단 후 재시작에서 복구.

### V2 에도 들어가지 않는다 (NOT IN — V3 이후)

- **시스템 (부팅) 디스크 작업** — V3. WinPE 부팅 환경 필요.
- **MBR ↔ GPT 변환** — V3.
- **다이나믹 디스크 / Storage Spaces** — 보류.
- **파티션 복구** — 별도 도구 영역.
- **디스크 클론 / 이미지** — 별도 도구 영역.

### 명시적 비목표 (Non-Goals)

- "EaseUS 와 100% 기능 동등" — 의도적으로 안 한다. 안전한 부분집합이 목표.
- "GUI 의 모든 작업이 즉시 실행" — 큰 이동은 분 단위 작업이고 그렇게 보여야 한다.
- "force / skip-validation 옵션" — V1 정책 그대로 유지.

---

## 3. 절대 규칙

### V1 에서 계승

CLAUDE.md 의 모든 절대 규칙은 V2 에서도 그대로 적용된다 — 4단계 패턴, 트랜잭션 로그, 시스템
디스크 보호, `unwrap()` / `expect()` 금지, 안전 가드 우회 금지.

### V2 추가 규칙

1. **알파 게이트** — V2 destructive 기능은 환경변수 `PARQ_ENABLE_V2_DESTRUCTIVE=1`
   없이는 절대 실행되지 않는다. UI 에서도 비활성. 우연한 활성화 차단.
2. **kill-test 통과 필수** — 모든 V2 destructive 작업은 "임의 시점에 프로세스 강제 종료 후
   재시작 → 데이터 무결성 검증" 테스트를 통과해야 마스터 머지 가능.
3. **VHD-only 테스트 강제** — V2 destructive 코드의 자동화 테스트는 VHD 위에서만. 실제 디스크는
   사용자 명시적 트리거로만 (CLI example 또는 GUI 의 알파 게이트 통과 후).
4. **checkpoint 포맷 명세 선행** — `docs/v2-checkpoint-format.md` 가 머지된 후에만 move 엔진
   코드 작성 가능.
5. **raw I/O 는 PR 분리** — `\\.\PhysicalDriveN` write 코드는 read 코드와 별도 PR.
   read 검증 통과 후 write PR.
6. **인접 파티션 무결성 보장** — 이동 작업 중 인접 파티션의 sector 1 byte 도 변경되지 않아야
   한다 (해시 비교로 검증).
7. **체크섬 라운드트립** — 이동 전후 데이터 영역 SHA256 일치 확인. 불일치 시 즉시 alarm + 롤백
   시도 + 사용자에게 raw 데이터 보존 위치 안내.

---

## 4. 단계별 진입 (Phase 0–4)

### Phase 0 — 헌장 / 설계 (코드 0줄)

- [x] `docs/v2-charter.md` (이 문서)
- [x] `docs/v2-move-algorithm.md` — libparted/GParted 의 forward / backward / split-copy 분석,
      Parq 가 거부할 케이스, 정렬 / cluster 경계 처리 *(draft 2026-07-09, 리뷰 대기)*
- [x] `docs/v2-checkpoint-format.md` — 트랜잭션 로그 v2 스키마, "in-progress region" 표기,
      fsync 순서, 복구 알고리즘 *(draft 2026-07-09, 리뷰 대기)*
- [x] `docs/v2-raw-io.md` — Windows raw I/O 인벤토리: `\\.\PhysicalDriveN`, `IOCTL_DISK_*`,
      `FSCTL_LOCK_VOLUME`, alignment, cache flush, MS 문서 / windows-rs API 매핑
      *(draft 2026-07-09, 리뷰 대기)*
- [x] `docs/v2-test-infrastructure.md` — VHD 매트릭스 (사이즈 × FS × 상태), kill-test 하네스
      설계, 무결성 검증 도구 *(draft 2026-07-09, 리뷰 대기)*

### Phase 1 — 테스트 인프라 (read-only / non-destructive 코드만)

- VHD 생성 매트릭스 스크립트 (`scripts/vhd-matrix.ps1`)
- kill-test 하네스 (Rust harness binary, 자식 프로세스 강제 종료 + 재시작)
- 디스크 영역 해시 검증 도구 (`scripts/hash-region.ps1`)
- 통합 테스트 framework 확장: VHD lifecycle 자동화

### Phase 2 — Raw I/O 파운데이션 (read-only 먼저)

- `\\.\PhysicalDriveN` open / sector geometry 조회 (read-only) — PR1
- `FSCTL_LOCK_VOLUME` / `FSCTL_DISMOUNT_VOLUME` 래퍼 (락 해제까지) — PR2
- read-only sector 읽기 + 해시 — PR3
- **여기까지 통과 후** 별도 PR 로 write 함수 — PR4

### Phase 3 — Move 엔진 (gated, VHD-only)

- chunked sector copy 함수 (checkpoint 마다 fsync) — PR5
- forward / backward copy 분기 결정 로직 — PR6
- checkpoint 기록 / 복구 로직 — PR7
- partition table 갱신 (move 완료 후 시작 LBA 업데이트) — PR8
- kill-test 매트릭스 통과 — Phase 3 완료 게이트

### Phase 4 — UI 통합

- PartitionBar 의 드래그 위치 조정 활성화
- 진행률 / ETA 표시
- "취소" 버튼 (중단 시 partial state 정리)
- 알파 게이트 ON 시에만 UI 노출

---

## 5. 게이팅 / 환경변수

| 변수 | 용도 | V1 | V2 |
|------|------|----|----|
| `PARQ_DEV_ALLOW_INTERNAL_DISKS` | bus-type 화이트리스트 우회 | 유지 | 유지 |
| `PARQ_ENABLE_V2_DESTRUCTIVE` | V2 destructive 기능 활성화 | — | **신규** |

V2 destructive 코드 경로는 `safety::v2_enabled()` 가 `false` 면 항상 거부 (V1 기능엔 영향 없음).

---

## 6. 의존성 정책

CLAUDE.md 상 새 의존성은 사용자 승인 필수. V2 진입에 따라 다음 두 개 사전 승인 (2026-05-09):

- `windows-rs` — IOCTL / FSCTL 호출용. Microsoft 공식. **승인됨**.
- `chrono` — 트랜잭션 로그 사람읽기용 ISO-8601. **승인됨** (V1.x 에서 보류됐던 것 V2 와 묶어 진행).
- 그 외 새 crate 는 기존 정책대로: PR 마다 명시적 승인.

`unsafe` 정책: V2 raw I/O 는 unavoidably `unsafe` 블록 사용. 모든 unsafe 블록은:
- `// SAFETY:` 주석 필수
- 작은 함수로 격리
- 안전한 래퍼 함수 export
- `#[deny(unsafe_op_in_unsafe_fn)]` 적용

---

## 7. 테스트 정책

### 자동화 (CI / 로컬 cargo test)

- VHD only — 절대 실제 디스크 안 만짐
- single-threaded 강제 (`--test-threads=1`)
- kill-test 매트릭스: forward-copy / backward-copy / cross-partition / mid-checkpoint 종료
- 무결성: 이동 전후 SHA256 + 인접 파티션 SHA256 무변경 검증

### 수동 (사용자 머신)

- 알파 게이트 ON + VM Disk #1 (10GB blank NVMe) 에서만
- 실제 디스크 (USB / 외장) 는 광범위 베타 테스트 후
- CI 에서는 admin 권한 destructive 테스트 절대 실행 안 함

---

## 8. CLAUDE.md 관계

- CLAUDE.md 의 V1 절대 규칙은 그대로 유지.
- V1 스코프 표("V1 에 없음")는 갱신: "파티션 이동" 항목 옆에 *"V2 에서 추가 (이 문서)"* 표기.
- V2 절대 규칙 (3 절) 은 CLAUDE.md 에 발췌 인용 + 본 문서 링크.
- 작업 시작 전 체크리스트에 V2 추가 항목:
  - "V2 destructive 인가? → 알파 게이트 통과? checkpoint 명세 머지됨? kill-test 작성됨?"

---

## 9. 결정된 정책 (2026-05-09 sign-off)

- **알파 게이트 환경변수명**: `PARQ_ENABLE_V2_DESTRUCTIVE` 확정.
- **windows-rs 의존성**: V2 raw I/O 위해 추가 승인.
- **chrono 의존성**: 트랜잭션 로그 ISO-8601 위해 추가 승인 (V1.x 보류분 V2 와 묶음).
- **트랜잭션 로그 포맷 마이그레이션**: V2 로그는 `log_format_version: 2` 필드로 분기. V1 로그
  (`log_format_version` 부재 또는 `1`) 는 그대로 read-only 표시 — 마이그레이션 / 재기록 안 함.
- **GPL-3.0 확정**: README 의 "예정" 표기 제거, 확정으로 변경.
- **알고리즘 차용 출처**: **libparted 1차 + GParted 보조**. KDE Partition Manager 는 cross-check.
  주의: 알고리즘은 저작권 대상이 아니므로 "분석 후 자체 구현". libparted 코드 직접 복붙 금지.
  Parq 자체 라이선스 (GPL-3.0) 와는 별개의 신중함이 필요 — 모든 V2 코드는 우리 손으로 작성됐고,
  외부 코드 차용 시 출처 / 라이선스 명시.

---

## 10. 다음 액션

Charter ratified. 다음:
1. ~~README.md GPL-3.0 확정 표기~~ (완료).
2. ~~CLAUDE.md 에 V2 hook 추가~~ (완료).
3. ~~Phase 0 의 나머지 4개 문서 작성~~ — **4개 draft 작성 완료 (2026-07-09).** 리뷰 후 머지.

Phase 1 코드는 4개 설계 문서 모두 머지된 후에만 시작. **현재: draft 4개 리뷰 대기 → 리뷰/커밋
후 Phase 1 착수 가능.**

---

*V2 는 사용자가 우리에게 더 위험한 작업을 맡기는 것이다. 신뢰 비용은 V1 보다 비싸다.*
