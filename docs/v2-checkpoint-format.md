# Parq V2 — Checkpoint 포맷 & 복구

> Phase 0 설계 문서 (3/5). **코드 0줄.** charter §3-2·§3-4·§3-7·§9 가 못박은 "kill-test 통과",
> "checkpoint 명세 선행", "체크섬 라운드트립"을 실제 파일 스키마와 복구 알고리즘으로 구체화한다.

**상태**: Draft — 리뷰 대기
**작성일**: 2026-07-09
**선행**: `v2-charter.md`, `v2-raw-io.md`
**게이트**: charter §3-4 — **이 문서가 머지된 후에만 move 엔진 코드 작성 가능.**

---

## 0. 문제 정의

파티션 이동은 수 GB 를 섹터 단위로 복사하는 **분 단위 작업**이다. 그 도중 전원이 끊기거나
프로세스가 죽으면, 디스크는 "원본 일부 + 대상 일부"가 공존하는 중간 상태로 남는다. 이때 필요한 것:

1. **어디까지 복사됐는지** 매체에 기록돼 있어야 한다 (checkpoint).
2. 재시작 시 그 기록을 읽어 **안전하게 이어가거나 / 안전하게 되돌리거나 / 최소한 사용자에게
   원본 데이터 위치를 알려줄** 수 있어야 한다.
3. 어떤 순간에 죽어도 **커밋된 데이터가 손상되지 않아야** 한다 (crash-consistency).

V1 트랜잭션 로그(`transaction/mod.rs`)는 "무슨 작업을 했다"는 감사 로그였지 복구용이 아니었다.
V2 는 이를 **복구 가능한 checkpoint 로그**로 확장한다.

---

## 1. V1 로그와의 관계 (charter §9)

현재 `TransactionLog`(transaction/mod.rs)에는 `log_format_version` 필드가 **없다.** charter §9 결정:

- V2 로그: `log_format_version: 2` 필드 **추가**.
- V1 로그: 필드 부재 또는 `1` → **그대로 read-only 표시. 마이그레이션/재기록 안 함.**

구현 방침:
```
// 확장된 TransactionLog (개념 스키마 — Phase 3 구현)
{
  "log_format_version": 2,          // 신규. 없으면 V1 로 간주
  "id": "...", "operation": "move_partition",
  "started_at_unix_nanos": ..., "disk_id": "...", ...   // V1 필드 그대로
  "steps": [ ... ],                 // V1 Step 그대로 (감사용 유지)
  "checkpoint": { ... }             // 신규. §2. move 계열만 존재
}
```

- `list_logs()`(transaction/mod.rs)는 이미 손상 파일을 스킵하고 read-only 로 파싱한다 — V2 필드는
  `#[serde(default)]` 로 붙여 **V1 로그 파싱을 깨지 않는다.**
- 역방향: V2 로그를 V1 바이너리가 읽어도 미지 필드는 serde 가 무시 → 최소한 감사 정보는 보임.

---

## 2. Checkpoint 스키마

이동은 원본 영역 `[src_lba, src_lba+len)` 을 대상 영역 `[dst_lba, dst_lba+len)` 로 청크 단위 복사한다.
checkpoint 는 "그 복사가 어디까지 매체에 확정됐는가"를 기록한다.

```
checkpoint = {
  "checkpoint_version": 1,

  // 이동 기하 (plan 확정 시 기록, 이후 불변)
  "geometry": {
    "disk_id": "…",                  // 안정 식별자 (raw-io §1 재확인 대상)
    "logical_sector_bytes": 512,     // raw-io §2.2
    "src_lba": 2048,                 // 원본 시작 (논리 섹터)
    "dst_lba": 411648,               // 대상 시작
    "length_sectors": 409600,        // 이동 길이
    "direction": "forward",          // "forward" | "backward" (move-algorithm §3)
    "chunk_sectors": 2048            // 청크 크기 (raw-io §3, 1 MiB @ 512)
  },

  // 진행 커서 (청크마다 갱신)
  "progress": {
    "chunks_total": 200,
    "chunks_done": 137,              // 이 수만큼은 매체에 확정됨(=fsync 완료)
    "next_chunk_index": 137,         // 재개 지점
    "phase": "copying"               // "planning"|"locked"|"copying"|"verifying"|"verified"|"table_update"|"done"|"aborting"
  },

  // 무결성 (charter §3-7)
  "integrity": {
    "src_sha256": "…",               // 이동 전 원본 데이터 영역 전체 해시 (plan 시 계산)
    "adjacent_hashes": [             // 인접/비대상 파티션 무변경 검증 (charter §3-6)
      { "partition_id": "…", "range_sectors": [0, 2048], "sha256": "…" }
    ],
    "verified_sha256": null          // 이동 완료 후 대상에서 재계산. src 와 일치해야 성공
  }
}
```

### 필드 규칙

- **`geometry` 는 planning 이후 불변.** 어떤 청크가 어디로 가는지는 결정론적이어서, 커서만 있으면
  재개 위치가 유일하게 정해진다.
- **`chunks_done` = 매체 확정 개수.** "쓰기 시작"이 아니라 "fsync 완료"의 카운트. (§3 write 순서)
- **overlap 처리**: src/dst 영역이 겹치면 direction 이 forward/backward 를 가른다 —
  `move-algorithm.md §3` 이 방향 결정, 여기선 그 방향대로 청크 인덱스→LBA 매핑만 기록.

---

## 3. Write 순서 & fsync 프로토콜 (crash-consistency 의 핵심)

**불변식(invariant): 어떤 순간에 죽어도, `chunks_done` 이 가리키는 지점까지의 대상 데이터는
매체에 확정돼 있고 손상되지 않았다.** 이를 보장하는 청크 1개당 순서:

```
for chunk i in [next_chunk_index .. chunks_total):
  1. src 에서 청크 read  (raw-io §2.3, NO_BUFFERING)
  2. dst 에 청크 write   (WRITE_THROUGH)
  3. FlushFileBuffers(disk)          ← 데이터가 매체 도달 확정 (raw-io §3)
  4. checkpoint.progress.chunks_done = i+1
     checkpoint.progress.next_chunk_index = i+1
  5. 로그 파일 write + fsync (기존 write_to_disk 의 tmp→rename→sync_all 패턴 재사용)
```

**순서가 전부다:**
- 3(데이터 flush)이 4·5(메타데이터 기록)보다 **먼저**. 데이터보다 커서가 앞서면, 재개 시 "썼다고
  기록됐지만 실제로는 안 쓰인" 청크가 생겨 무결성이 깨진다. 데이터가 커서보다 앞서는 건 안전
  (재개 시 그 청크를 한 번 더 쓸 뿐, 멱등).
- 5 는 V1 `write_to_disk()` 의 **atomic 교체(tmp→rename)+sync_all** 을 그대로 쓴다 — 로그 파일
  자체가 반쯤 쓰이는 일은 없다.
- **로그와 데이터는 다른 디스크에 있어야 한다.** 로그는 `%LOCALAPPDATA%`(시스템 디스크), 이동
  대상은 외장/VM Disk#1. 같은 디스크면 이동이 로그를 밟을 수 있다. → 복구 전제 조건으로 강제.

### forward copy 의 데이터 안전성 문제

src/dst 가 겹치고 dst > src (forward) 이면, 앞에서부터 복사할 때 **아직 안 읽은 src 영역을 이미
쓴 dst 가 덮어쓸 수 있다.** 이 겹침/방향 규칙은 `move-algorithm.md §3` 이 결정하며, checkpoint 는
그 결정을 신뢰하고 방향대로만 진행한다. (backward copy 는 뒤에서부터 → 반대 안전.)

---

## 4. 복구 알고리즘 (재시작 시)

재시작 시 `list_logs()` 로 `result == None` 이고 `log_format_version == 2` 이며 `checkpoint` 가
있는 로그를 찾는다. `phase` 별 복구:

| `phase` | 죽은 시점 의미 | 복구 동작 |
|---------|----------------|-----------|
| `planning` | 아직 아무것도 안 씀 | 로그 `aborted_before_write` 로 마감. 디스크 무변경. 안전. |
| `locked` | lock 만 잡음, 복사 전 | 동일 — 무변경 마감. lock 은 프로세스 죽으며 해제됨. |
| `copying` | 청크 복사 중 | **재개 또는 롤백** (§4.1) |
| `verifying` | 복사 완료, 대상 SHA256 검증 중 | 대상 SHA256을 다시 계산하고 검증을 재개. |
| `verified` | 데이터와 인접 영역 검증 완료 | 파티션 이동이면 `table_update`로 진행. raw 영역 이동이면 완료 상태. |
| `table_update` | 데이터 복사 끝, 파티션 테이블 갱신 중 | §4.2 |
| `done` | 완료 후 로그 마감 전 | 로그만 `committed` 로 마감. |
| `aborting` | 이미 롤백 중이었음 | 롤백 재개. |

### 4.1 `copying` 단계 복구

전제: `chunks_done` 까지는 dst 에 확정. 원본 데이터는 아직 그대로다(테이블 미갱신 → src 가 여전히
공식 위치). 두 선택지:

- **재개(resume)** — `next_chunk_index` 부터 계속 복사. 기본 전략. 멱등(이미 쓴 청크 재쓰기 무해).
- **롤백(rollback)** — dst 에 부분 복사된 것을 버리고 src 를 그대로 둠. **비파괴적**: 아직 파티션
  테이블이 src 를 가리키므로, dst 부분 데이터는 "미할당 영역에 쓰인 쓰레기"일 뿐 원본은 온전.

**V2 기본 정책: `copying` 중 크래시 → 자동 재개 시도. 단 재개 전 `integrity.src_sha256` 로 원본
영역이 아직 안 망가졌는지 먼저 검증.** 검증 실패 시 재개 중단 + 사용자에게 원본 raw 위치 안내
(charter §3-7). overlap 이동에서 원본이 이미 부분 훼손됐을 수 있는 경우가 여기 해당 →
`move-algorithm.md` 가 "resume-safe / rollback-only" 케이스를 분류한다.

### 4.2 `table_update` 단계 복구

가장 위험한 창(window). 데이터는 dst 에 완전 복사됐고 검증도 통과했으나, 파티션 테이블 엔트리를
src→dst 로 바꾸는 중 죽음. 파티션 테이블 쓰기는 **단일 섹터 원자적 갱신**으로 설계(테이블 섹터
하나를 통째로 준비해 1회 write+flush). 따라서:

- 테이블 섹터가 **구(src) 상태**면 → 데이터는 dst 에 있지만 공식 위치는 src. dst 데이터를 src 로
  되돌릴 필요 없음(src 원본 온전). 테이블을 dst 로 다시 쓰거나(재개), 그냥 src 로 두거나(롤백).
- 테이블 섹터가 **신(dst) 상태**면 → 이동 성공. 로그만 마감.
- 테이블 섹터가 **반쯤 쓰임**? → NO_BUFFERING+단일섹터+flush 로 "반쯤"을 원천 차단하는 게 설계
  목표. 그럼에도 방어적으로: plan 시 원본 테이블 섹터를 로그에 백업(`table_backup` 필드, base64)
  해 두고, 재계산으로 유효성 확인 후 복원.

> 결론: 데이터 복사(§4.1)와 테이블 갱신(§4.2) **사이에 반드시 무결성 검증(§5)을 통과**시킨다.
> 즉 "src 원본은 검증 완료 전까지 절대 건드리지 않는다" — 원본 삭제/해제는 이동 성공 확정
> **이후** 별도 단계. 이게 charter §3-7 "롤백 + raw 데이터 보존"의 구조적 보증.

---

## 5. 무결성 검증 (charter §3-6, §3-7)

이동 성공으로 간주하기 전 **모두** 통과해야 한다:

1. **라운드트립**: `verified_sha256`(dst 데이터 영역 재계산) == `src_sha256`. 불일치 → 즉시 alarm +
   롤백 + 원본(src) 위치 사용자 안내. dst 를 공식 위치로 승격하지 않음.
2. **인접 무변경**: `adjacent_hashes` 의 각 파티션/범위를 재계산 → plan 시 값과 **완전 일치**.
   1 byte 라도 다르면 실패 (charter §3-6). 이동이 인접을 밟았다는 뜻 = 심각 버그.
3. 검증 통과 후에만 §4.2 테이블 갱신 진행.

해시 계산 도구/전략은 `v2-test-infrastructure.md` 가 (kill-test 하네스와 공유) 정의.

---

## 6. kill-test 대응 (charter §3-2)

이 포맷이 kill-test 를 통과시키는 방식:

- 하네스가 **임의 청크 경계·비경계에서 프로세스 SIGKILL** → 재시작 → §4 복구 → 무결성 §5.
- 검증 매트릭스(= `v2-test-infrastructure.md` 가 실행):
  - `copying` 중 kill × {forward, backward} × {overlap, non-overlap}
  - §3 순서의 3·4 **사이**(데이터 flush 후 / 커서 기록 전) kill → 재개 시 청크 재쓰기 멱등 확인
- `table_update` 중 kill → §4.2 테이블 3-상태 각각
- 구현된 MBR 경로는 테이블 write 직전과 단일 sector write+flush 직후를 각각 강제 종료해 재개 검증
- **통과 기준**: 위 모든 시나리오에서 최종 상태가 (a) 완전 이동 성공+무결성 통과, 또는 (b) 완전
  원상복구(src 온전) 중 하나. **"중간 손상"은 0건이어야 머지.**

---

## 7. 미결정 / 후속

- **테이블 백업 필드 크기**: MBR 은 섹터 1개, GPT 는 헤더+엔트리 배열 → 로그 비대. GPT 는 관련
  섹터만 선별 백업할지 Phase 3 에서 결정.
- **resume vs rollback 기본값 UI 노출**: charter §2 "force 없음" 정신상 자동 재개가 기본. 단
  `integrity.src_sha256` 실패 시엔 자동 진행 금지, 사용자 명시 선택. UI 문구는 Phase 4.
- **해시 알고리즘**: SHA256 확정(charter 문구). 대용량 성능이 문제되면 청크별 해시 트리 도입 검토
  (후속, 포맷 `checkpoint_version` 2 로).

---

*checkpoint 은 "전원이 끊긴 순간"과 사용자 데이터 사이의 유일한 계약서다. 이 스키마가 거짓말을
하면(썼다는데 안 썼으면) 복구가 데이터를 지운다. 그래서 §3 의 순서는 타협 불가다.*
