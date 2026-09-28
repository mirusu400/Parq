# Parq V2 — 파티션 이동 알고리즘

> Phase 0 설계 문서 (4/5). **코드 0줄.** libparted / GParted 의 이동 전략을 분석하고, Parq 가
> **수행할 케이스와 거부할 케이스**를 명시한다. charter §9 결정대로 **libparted 1차 + GParted
> 보조 + KDE Partition Manager cross-check** 로 알고리즘을 분석하되, 코드는 우리 손으로 작성한다
> (직접 복붙 금지).

**상태**: Draft — 리뷰 대기
**작성일**: 2026-07-09
**선행**: `v2-charter.md`, `v2-raw-io.md`, `v2-checkpoint-format.md`
**참조**: [GParted](https://gitlab.gnome.org/GNOME/gparted), [libparted](https://www.gnu.org/software/parted/), [KDE Partition Manager](https://github.com/KDE/partitionmanager)

---

## 0. "이동"이 V1 리사이즈와 다른 점

V1 리사이즈(`partition/mod.rs::execute_resize_partition`)는 Windows `Resize-Partition` 래핑이고,
**끝 경계만** 움직인다 — 시작 LBA 불변. 그래서 파일시스템이 자기 데이터를 알아서 재배치한다
(무손실, 우리는 섹터를 안 건드림).

**이동은 시작 LBA 를 바꾼다.** Windows 는 이걸 지원하는 API 가 없다. 그래서 우리가 직접
파일시스템 전체(사용/미사용 무관, 파티션 raw 이미지 전부)를 새 위치로 **바이트 정확히 복사**하고,
파티션 테이블의 시작 LBA 엔트리를 갱신한다. 파일시스템 내부는 시작 위치를 신경 안 쓰므로(대부분
LBA 상대가 아닌 파티션 상대 주소 사용) 복사만 정확하면 그대로 마운트된다.

> **범위 한정**: V2 이동은 파티션의 **raw 섹터 전체를 그대로** 옮긴다. 파일시스템을 "이해"해서
> 사용 블록만 옮기는 최적화(GParted 의 일부 FS-aware copy)는 **하지 않는다.** 전체 복사가 느려도
> 안전이 우선 (charter §2 비목표: "즉시 실행" 아님, 분 단위 정상).

---

## 1. libparted / GParted 전략 요약 (분석)

분석 결과 이동의 핵심은 **원본과 대상 영역의 겹침(overlap) 여부와 방향**이다.

- **libparted `ped_disk_*` + `ped_file_system_*`**: 파일시스템을 리사이즈/복사할 때 소스→대상
  블록 복사 루프를 돌며, 겹칠 때 방향을 골라 자기잠식을 피한다.
- **GParted `copy_filesystem` / `maximize_filesystem`**: 블록 버퍼(수 MiB)를 잡고 read/write 루프.
  진행률 보고. 겹침 이동에서 forward/backward 를 선택.
- **공통 원리** (Parq 가 채택): 겹치는 이동에서
  - `dst > src` (뒤로 이동, 오른쪽으로): **backward copy** (마지막 청크부터 복사).
  - `dst < src` (앞으로 이동, 왼쪽으로): **forward copy** (첫 청크부터 복사).
  - 겹치지 않으면(non-overlap): 방향 무관, forward 로 통일(구현 단순).

이 방향 규칙이 checkpoint 의 `direction` 필드(`v2-checkpoint-format.md §2`)를 결정한다.

---

## 2. 겹침과 방향 — 왜 이게 안전의 전부인가

영역 `src=[s, s+L)`, `dst=[d, d+L)`, 청크 크기 `c`. 겹치면 한쪽을 쓰는 순간 다른 쪽의 아직-안-읽은
데이터를 파괴할 수 있다.

```
경우 A: dst < src (왼쪽으로 이동), 겹침
   src:        [========]
   dst:    [========]
   → forward (앞→뒤): 청크0 을 dst 앞부분에 쓸 때, 그 영역은 src 의 "이미 읽은" 앞부분 뒤이므로
     아직 안 읽은 src 뒷부분을 건드리지 않는다. 안전.

경우 B: dst > src (오른쪽으로 이동), 겹침
   src:    [========]
   dst:        [========]
   → backward (뒤→앞): 마지막 청크를 dst 뒷부분에 쓸 때, src 의 아직 안 읽은 앞부분을 안 건드림.
     forward 로 하면 dst 앞부분 write 가 src 뒷부분(아직 읽어야 할)을 덮어써 파괴. 그래서 backward.
```

**규칙 (Parq 확정):**
```
if overlap(src, dst):
    direction = (dst > src) ? backward : forward
else:
    direction = forward
```

이 규칙이 지켜지면 **이동 도중에도 원본의 아직-안-읽은 부분은 항상 온전**하다 →
`v2-checkpoint-format.md §4.1` 의 "resume-safe" 성질의 근거.

---

## 3. resume-safe vs rollback-only 분류

checkpoint 복구(§4.1)가 자동 재개해도 되는지는 겹침에 달렸다:

| 케이스 | 원본 훼손 여부 | 복구 정책 |
|--------|----------------|-----------|
| **non-overlap** | 이동 중 src 전혀 안 변함 | **resume-safe**. 언제 죽어도 src 온전 → 재개/롤백 모두 안전 |
| **overlap, 방향 올바름** | 이미 복사·확정된 앞(또는 뒤) 청크에 해당하는 src 영역은 dst 와 겹쳐 덮였을 수 있음 | **조건부 resume**. `chunks_done` 까지는 dst 에 안전 확정. 재개는 `next_chunk_index`부터 → src 의 미읽은 부분만 읽음(그 부분은 아직 온전, §2). 안전 |
| **overlap, 방향 위반(버그)** | 미읽은 src 파괴 가능 | 발생하면 안 됨. 발생 시 rollback-only + raw 보존 안내 |

**핵심 보증**: §2 방향 규칙 + `chunks_done`=매체확정 커서 ⇒ overlap 이어도 "재개 시 읽어야 할 src
영역은 항상 미훼손". 따라서 overlap 이동도 **조건부 resume-safe**. 재개 전에는 완료 청크를 dst,
미완료 청크를 src에서 읽어 최초 스냅샷 SHA를 재구성한다. 재구성 해시가 다르면 외부 쓰기나
하드웨어 오류가 있었던 것이므로 추가 쓰기 전에 중단한다.

> non-overlap 이동을 **선호**: plan 단계에서 가능하면 겹치지 않는 목적지를 고른다(중간에 충분한
> 미할당 공간이 있으면). 겹침은 안전하긴 하나 복구 추론이 복잡 → 피할 수 있으면 피한다.

---

## 4. 정렬 / cluster 경계 처리

이동은 **파티션 raw 전체 복사**라 파일시스템 cluster 경계는 (내부적으로는) 신경 쓸 필요 없다 —
src/dst 파티션 크기가 같고 내용이 바이트 동일하면 FS 는 그대로 유효. 하지만:

1. **파티션 시작 정렬**: dst_lba 는 **1 MiB(2048 섹터 @512) 경계**에 맞춘다. Windows/현대 디스크
   표준. 미정렬 시작은 성능 저하 + 일부 도구 경고. plan 이 dst_lba 를 정렬 강제.
2. **섹터 정렬**: 모든 I/O 는 논리 섹터 배수 (`v2-raw-io.md §3`). L(길이)은 파티션 섹터 수 그대로.
3. **4Kn vs 512e**: 논리 섹터 크기에 따라 청크 섹터 수만 바뀜(1 MiB 유지). 알고리즘 불변.
4. **파티션 테이블 재계산**: 이동 후 시작 LBA 만 바뀌고 길이는 불변. MBR CHS/LBA 필드, GPT
   `FirstLBA`/`LastLBA` + **CRC32 재계산 + 백업 GPT 헤더 동기화** 필요 → GPT 는 §6 별도 주의.

---

## 5. Parq 가 **거부**하는 케이스 (plan 단계 실패)

안전을 위해 다음은 이동 plan 자체를 거부한다:

- **시스템/부팅 디스크·파티션** — charter §2, V1 safety 가드 그대로 상속. V2 도 시스템 디스크 금지
  (그건 V3).
- **마운트/lock 실패 볼륨** — `v2-raw-io.md §4` lock 못 잡으면 거부.
- **대상 영역이 다른 파티션과 겹침** — 미할당 공간이 아닌 곳으로 이동 요청 → 거부.
- **대상 영역이 디스크 범위 초과** — `dst_lba + L > disk_sectors` → 거부.
- **논리 섹터 크기 불명 / geometry 조회 실패** — 안전 판단 불가 → 거부.
- **지원 안 하는 파티션 스타일** — RAW, 다이나믹 디스크 → 거부(charter).
- **`PARQ_ENABLE_V2_DESTRUCTIVE` 미설정** — charter §3-1 알파 게이트. plan 은 계산 가능하나
  execute 거부. (plan 은 read-only 라 게이트 밖에서도 미리보기 허용할지는 §7 미결정.)
- **로그와 이동 대상이 같은 디스크** — `v2-checkpoint-format.md §3` 복구 전제 위반 → 거부.

거부는 전부 `ParqError::ValidationFailed` / `SystemPartitionProtected` (기존 타입 재사용).

---

## 6. GPT 특별 주의

MBR 은 파티션 엔트리 하나(시작 LBA + 길이)만 갱신하면 되지만 GPT 는:

- **Primary GPT**(LBA1) 와 **Backup GPT**(디스크 끝) **양쪽** 엔트리 배열 갱신.
- 엔트리 배열 CRC32, 헤더 CRC32 **재계산**.
- 두 헤더가 서로를 가리키는 필드 일관성.

→ 이동의 파티션 테이블 갱신은 MBR 보다 GPT 가 훨씬 조심스럽다. `v2-checkpoint-format.md §4.2`
의 "단일 섹터 원자적 갱신"이 GPT 에선 여러 섹터라 **다단계**가 된다. 정책:

1. 데이터 복사·검증 완료 후, backup GPT 먼저 갱신 → flush.
2. primary GPT 갱신 → flush.
3. 각 단계를 checkpoint `phase` 세분(`table_update_backup`/`table_update_primary`)해 복구 지점 명확화.
4. 중간 죽음 시: primary 가 구(舊)면 전체가 아직 src → 롤백/재개 안전. primary 가 신(新)이면 성공.

> 구현은 backup GPT → primary GPT 순서와 각 단계 checkpoint를 사용하며 실제 VHD 강제종료
> 복구 매트릭스로 검증한다.

---

## 7. 미결정 / 후속

- **plan 미리보기의 게이트 위치**: plan(read-only)을 알파 게이트 밖에서 허용해 UI 미리보기를 줄지,
  아니면 게이트 안에서만 계산할지. UX vs 보수성. Phase 4 에서 확정.
- **FS-aware 최적화 도입 여부**: 전체 raw 복사 대신 사용 블록만 — 성능 크게 개선되나 FS 파싱
  위험 급증. **V2 범위 밖.** V2.x 이후 별도 charter 필요.
- **이동 + 리사이즈 합성**(shrink-then-move 등, charter §2 IN-3): 각 원자 작업(리사이즈=V1 검증됨,
  이동=V2)을 **순차 트랜잭션**으로 엮되, 중간 실패 시 각 단계 독립 복구. 세부는 이동 단독이
  kill-test 통과 후 별도 설계 노트.
- **진행률/ETA 모델**: `chunks_done/chunks_total` 기반 선형 추정. Phase 4 UI.

---

## 8. 구현 순서 (charter §4 Phase 3 재확인)

```
PR5: chunked sector copy (checkpoint 마다 fsync)      ← v2-raw-io §3 + checkpoint §3
PR6: forward/backward 방향 결정 로직 (§2)              ← 이 문서 §2·§3
PR7: checkpoint 기록/복구 (checkpoint §4)              ← v2-checkpoint-format 전체
PR8a: MBR 파티션 테이블 갱신 (§4·§6)
PR8b: GPT 파티션 테이블 갱신 (§6)
Phase3 게이트: kill-test 매트릭스 전부 통과 (v2-test-infrastructure)
```

각 PR 은 VHD-only 자동 테스트(charter §3-3) + `PARQ_ENABLE_V2_DESTRUCTIVE` 게이트 뒤.

---

*이동은 "사용자 데이터를 통째로 들어 다른 곳에 내려놓는" 작업이다. 겹침·방향·정렬 셋 중 하나만
틀려도 데이터가 사라진다. 그래서 §2 방향 규칙은 이 문서의 심장이다 — 다른 모든 안전장치는
그 위에 얹힌 이중화일 뿐이다.*
