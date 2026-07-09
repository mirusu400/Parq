# Parq V2 — 테스트 인프라

> Phase 0 설계 문서 (5/5). **코드 0줄.** charter §3-2·§3-3·§7 의 "kill-test 통과", "VHD-only
> 자동 테스트", "무결성 검증"을 실제 하네스/스크립트/매트릭스로 구체화한다. 이 문서가 머지되면
> Phase 0 완료 → Phase 1(테스트 인프라 코드, non-destructive) 착수 가능.

**상태**: Draft — 리뷰 대기
**작성일**: 2026-07-09
**선행**: `v2-charter.md`, `v2-raw-io.md`, `v2-checkpoint-format.md`, `v2-move-algorithm.md`

---

## 0. 원칙 (charter §7 재확인)

- **자동화 테스트는 VHD 위에서만.** 실제 물리 디스크는 절대 안 건드림. (기존 `create-test-vhd.ps1`
  이 이미 물리 드라이브 경로를 거부하는 가드를 가짐 — V2 도 이 정신 계승.)
- **single-threaded 강제** (`--test-threads=1`). 통합 테스트가 디스크 상태를 공유.
- **CI 에서 admin 권한 destructive 테스트 절대 실행 안 함.** kill-test/이동 테스트는 로컬 개발자
  머신(관리자 PowerShell) 또는 self-hosted 러너에서만.
- 수동 실기기 테스트는 **VM Disk #1 (10GB blank NVMe)** 에서 `PARQ_ENABLE_V2_DESTRUCTIVE=1` +
  `PARQ_DEV_ALLOW_INTERNAL_DISKS=1` 로만. (사용자 환경 메모 상 이미 존재하는 대상.)

---

## 1. VHD 매트릭스 (`scripts/vhd-matrix.ps1`, Phase 1)

기존 `create-test-vhd.ps1`(단일 10GB GPT + FAT32/exFAT/NTFS 시나리오)을 **매트릭스**로 확장한다.
이동 알고리즘의 분기(overlap/방향/정렬/섹터크기/파티션스타일)를 모두 커버해야 한다.

| 축 | 값 | 근거 문서 |
|----|----|-----------|
| 논리 섹터 | 512e, 4Kn (`New-VHD -LogicalSectorSizeBytes 4096`) | raw-io §2.2, move §4 |
| 파티션 스타일 | MBR, GPT | move §6 |
| 이동 방향 | 왼쪽(dst<src), 오른쪽(dst>src) | move §2 |
| 겹침 | non-overlap, overlap | move §2·§3 |
| FS | NTFS, FAT32, exFAT | (이동은 raw 복사라 FS 무관하나 마운트 재검증용) |
| 크기 | 소(64 MiB, 빠른 반복), 중(1 GiB, 현실적) | 속도 vs 현실성 |

- 매트릭스 전체 조합이 아니라 **커버링 세트**(각 값이 최소 1회 등장 + 위험 조합 강제 포함)를
  생성. 예: `4Kn × GPT × 오른쪽 × overlap` 은 반드시 포함(가장 까다로움).
- 각 VHD 는 **알려진 패턴 데이터**(예: LBA 번호를 섹터마다 기록)로 채워 무결성 비교를 쉽게 함.
- 스크립트는 생성 후 **각 파티션·미할당 영역의 시작 LBA/길이/SHA256 을 JSON manifest 로 출력**
  → kill-test 하네스가 이 manifest 를 기준값으로 사용.

```
# 개념 (Phase 1 구현)
.\scripts\vhd-matrix.ps1 -OutDir .\test-vhds\ -Cases covering
#  → test-vhds/case-01-512e-gpt-right-overlap.vhdx + case-01.manifest.json ...
```

---

## 2. 영역 해시 도구 (`scripts/hash-region.ps1` + Rust, Phase 1)

charter §7 무결성 = "이동 전후 SHA256 + 인접 파티션 무변경". 두 구현이 **같은 값**을 내야 한다
(교차 검증):

- **PowerShell** `hash-region.ps1 -DiskNumber N -StartLba L -LengthSectors C` — 독립 기준(oracle).
  raw 읽기는 .NET `FileStream` 으로 `\\.\PhysicalDriveN` 직접 open.
- **Rust** 동일 기능 — `v2-raw-io.md` read 파운데이션(PR3) 위에 구현. 이게 실제 제품 코드가 쓰는
  경로.

PR3 게이트(raw-io §7): 두 도구가 임의 영역에서 **SHA256 일치**해야 write 코드 착수 허용.

---

## 3. kill-test 하네스 (Rust binary, Phase 1)

charter §3-2 의 핵심. 구조: **부모(하네스) + 자식(이동 프로세스)**.

```
harness (test-only binary, VHD 대상):
  1. vhd-matrix 케이스 로드 + manifest 기준값 확보
  2. 자식 프로세스 spawn: 이동 실행 (PARQ_ENABLE_V2_DESTRUCTIVE=1, 대상 VHD)
  3. kill 트리거 도달 시 자식을 SIGKILL 급으로 강제 종료 (TerminateProcess)
  4. 자식 재시작 → checkpoint 복구 경로 실행 (v2-checkpoint-format §4)
  5. 최종 상태 판정: manifest 기준으로
        (a) 이동 성공 + src_sha256==verified_sha256 + 인접 무변경, 또는
        (b) 완전 원상복구(src 온전 + 인접 무변경)
     둘 중 하나여야 PASS. "중간 손상"이면 FAIL.
```

### kill 트리거 지점 (`v2-checkpoint-format.md §6` 매트릭스)

이동 프로세스에 **테스트 전용 kill-point 훅**을 심는다(제품 빌드엔 컴파일 제외, `#[cfg(feature =
"kill-test")]`). 트리거:

- `copying` 청크 경계 직후 (커서 기록 후)
- `copying` 청크 중간 (데이터 flush 전)
- **데이터 flush 후 / 커서 기록 전** (§3 순서의 3↔4 사이 — 멱등 재쓰기 검증의 핵심)
- `table_update` 진입 직후
- GPT: backup 갱신 후 / primary 갱신 전 (move §6)

각 트리거 × VHD 케이스(overlap/방향/섹터/스타일) = kill-test 매트릭스. **전부 PASS 여야 Phase 3
완료 게이트 통과.**

### 결정론

- kill-point 는 "N번째 청크에서 kill" 처럼 **결정론적 카운터**로 지정(랜덤 타이밍 아님) → 실패
  재현 가능. 랜덤 fuzzing 은 결정론 매트릭스 통과 후 보너스로.

---

## 4. 통합 테스트 프레임워크 확장 (Phase 1)

기존 `tests/integration/`(현재 스캐폴드)을 VHD lifecycle 자동화로 확장:

- **setup/teardown**: 케이스별 VHD attach → 테스트 → detach + 파일 삭제. `mount-test-vhd.ps1`
  로직을 Rust 테스트 하네스에서 호출(또는 포팅).
- **격리**: `PARQ_TRANSACTIONS_DIR` (기존 transaction 모듈이 이미 지원)로 각 테스트가 독립 로그
  디렉토리 사용 → 로그 충돌 방지.
- **게이트 존중**: 이동 테스트는 `PARQ_ENABLE_V2_DESTRUCTIVE=1` 을 테스트 하네스가 설정. 게이트
  자체 테스트(미설정 시 거부되는가?)도 포함.
- `--test-threads=1` 강제(charter §7). VHD 상태 공유라 병렬 금지.

---

## 5. CI 정책

| 잡 | 환경 | 실행 | destructive |
|----|------|------|-------------|
| 단위 테스트 (`cargo test --lib`) | GitHub-hosted | 매 PR | ✗ (순수 함수) |
| clippy / fmt | GitHub-hosted | 매 PR | ✗ |
| VHD 통합 + kill-test | **self-hosted (admin, Windows)** 또는 로컬 게이트 | Phase 1+ | ✓ VHD-only |

- GitHub-hosted 러너는 admin/Hyper-V VHD attach 가 제약 → destructive 잡은 self-hosted 러너 또는
  "로컬에서 통과 후 라벨" 방식. charter §7 "CI 에서 admin destructive 절대 실행 안 함"과 배치.
- 최소선: **kill-test 매트릭스 PASS 증적을 PR 에 첨부**(로그+manifest diff) 없이는 Phase 3 코드
  머지 금지.

---

## 6. Phase 1 산출물 체크리스트 (이 문서 머지 후 착수 가능)

- [x] `scripts/vhd-matrix.ps1` — 커버링 세트 VHD + manifest 생성 (§1) *(2026-07-09, 구문 검증 통과. 실기 실행 미검증 — admin+Hyper-V 필요)*
- [x] `scripts/hash-region.ps1` — PowerShell oracle 해시 (§2) *(2026-07-09, read-only, 구문 검증 통과)*
- [ ] kill-test 하네스 Rust binary (§3) — **단, 자식의 이동 코드는 아직 없음 → 하네스는 mock
      이동(단순 복사 루프)으로 먼저 자체 검증. 실제 이동 연결은 Phase 3.**
- [ ] 통합 테스트 VHD lifecycle 자동화 (§4)
- [ ] CI 잡 정의 (§5) — self-hosted 러너 셋업 문서 포함

**전부 non-destructive/read-only 또는 VHD-only 이므로 charter §4 Phase 1 규칙(‘read-only /
non-destructive 코드만’) 준수.** raw write 는 여전히 Phase 2 PR4 이후.

---

## 7. 미결정 / 후속

- **4Kn VHD 생성 가능성**: `New-VHD -LogicalSectorSizeBytes 4096 -PhysicalSectorSizeBytes 4096`
  가 이 VM 환경에서 되는지 Phase 1 착수 시 실측. 안 되면 4Kn 은 실기기 수동 테스트로 격하.
- **self-hosted 러너 유무**: 없으면 "로컬 통과 + 증적 첨부" 수동 게이트로 운영. 결정 필요.
- **mock 이동 vs 실제 이동 하네스 재사용**: kill-test 하네스를 Phase 1 에 mock 으로 만들고 Phase 3
  에서 실제 이동에 그대로 붙이는 인터페이스 설계 — Phase 1 PR 에서 확정.

---

*테스트 인프라는 "우리가 안전하다고 믿는 근거"다. kill-test 가 없으면 "전원 끊겨도 괜찮다"는 말은
희망사항일 뿐이다. 이 하네스가 그 희망을 증명(또는 반증)한다.*
