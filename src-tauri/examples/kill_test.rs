// kill-test 하네스 (mock). docs/v2-test-infrastructure.md §3, docs/v2-checkpoint-format.md §3·§4.
//
//   cargo run --example kill_test
//
// **디스크를 건드리지 않는다.** 실제 move 엔진이 아직 없으므로, 임시 파일 위에서 "청크 복사 +
// checkpoint" 를 mock 이동으로 돌려 하네스 메커니즘(자식 강제 종료 → 재시작 → 복구 → 무결성)을
// 자체 검증한다. 검증하는 불변식:
//
//   checkpoint write 순서(§3): (1)데이터 write → (2)flush → (3)커서(chunks_done) 기록 → fsync.
//   어느 시점에 죽어도 재시작 시 dst 가 src 와 byte-exact 로 복구돼야 한다(멱등 재복사).
//
// 한 바이너리가 부모(하네스)와 자식(mock mover)을 겸한다. PARQ_KILLTEST_CHILD=1 이면 자식.
// 자식은 KILL_AT_CHUNK / KILL_PHASE 로 지정된 결정론적 지점에서 process::abort() → 전원 차단 모사.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CHUNK_BYTES: usize = 4096;
const TOTAL_CHUNKS: u64 = 16;

#[derive(Serialize, Deserialize, Default)]
struct Checkpoint {
    chunks_done: u64,
    total: u64,
}

fn env_child() -> bool {
    std::env::var("PARQ_KILLTEST_CHILD").as_deref() == Ok("1")
}

fn main() {
    if env_child() {
        child_main();
    } else {
        std::process::exit(parent_main());
    }
}

// ---------- 자식: mock 이동 (checkpoint 순서 §3) ----------

fn child_main() {
    let src = PathBuf::from(std::env::var("KT_SRC").expect("KT_SRC"));
    let dst = PathBuf::from(std::env::var("KT_DST").expect("KT_DST"));
    let ckpt = PathBuf::from(std::env::var("KT_CKPT").expect("KT_CKPT"));
    let kill_at: Option<u64> = std::env::var("KILL_AT_CHUNK").ok().and_then(|s| s.parse().ok());
    // "before_cursor" = 데이터 flush 후 / 커서 기록 전 (§3 의 3↔4 사이). "after_cursor" = 커서 기록 후.
    let kill_phase = std::env::var("KILL_PHASE").unwrap_or_else(|_| "before_cursor".into());

    // checkpoint 로드 → 재개 지점 결정 (§4 copying 복구).
    let mut cp: Checkpoint = fs::read_to_string(&ckpt)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Checkpoint {
            chunks_done: 0,
            total: TOTAL_CHUNKS,
        });
    if cp.total == 0 {
        cp.total = TOTAL_CHUNKS;
    }

    let mut fsrc = File::open(&src).expect("open src");
    let mut fdst = OpenOptions::new().write(true).open(&dst).expect("open dst");

    let mut buf = vec![0u8; CHUNK_BYTES];
    for i in cp.chunks_done..cp.total {
        let off = i * CHUNK_BYTES as u64;

        // (1) src 청크 read
        fsrc.seek(SeekFrom::Start(off)).unwrap();
        fsrc.read_exact(&mut buf).unwrap();

        // (2) dst 청크 write + (3) flush (매체 도달 확정 모사)
        fdst.seek(SeekFrom::Start(off)).unwrap();
        fdst.write_all(&buf).unwrap();
        fdst.sync_all().unwrap();

        // kill-point: 데이터는 flush됐지만 커서는 아직 → 재시작 시 이 청크 재복사(멱등) 검증.
        if kill_at == Some(i) && kill_phase == "before_cursor" {
            std::process::abort();
        }

        // (4) 커서 전진 + fsync (§3: 데이터 다음에 커서)
        cp.chunks_done = i + 1;
        write_checkpoint(&ckpt, &cp);

        // kill-point: 커서까지 기록된 뒤 → 재시작 시 다음 청크부터 재개 검증.
        if kill_at == Some(i) && kill_phase == "after_cursor" {
            std::process::abort();
        }
    }
    // 정상 완료. (mock 이라 파티션 테이블 갱신 단계는 생략)
    std::process::exit(0);
}

fn write_checkpoint(path: &Path, cp: &Checkpoint) {
    // transaction/mod.rs 의 tmp→rename→fsync 패턴 재사용 (원자적 교체).
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec(cp).unwrap();
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
        .unwrap();
    f.write_all(&json).unwrap();
    f.sync_all().unwrap();
    drop(f);
    fs::rename(&tmp, path).unwrap();
}

// ---------- 부모: 하네스 ----------

struct Scenario {
    name: &'static str,
    kill_at: Option<u64>,
    phase: &'static str,
}

fn parent_main() -> i32 {
    let dir = std::env::temp_dir().join("parq-killtest");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let src = dir.join("src.bin");
    let dst = dir.join("dst.bin");
    let ckpt = dir.join("move.ckpt.json");

    // src: 청크마다 스탬프 패턴 (오프셋/정렬 오류 검출용). dst: 0 초기화.
    let src_sha = make_src(&src);

    let scenarios = [
        Scenario { name: "no-kill baseline", kill_at: None, phase: "before_cursor" },
        Scenario { name: "kill@0 before_cursor", kill_at: Some(0), phase: "before_cursor" },
        Scenario { name: "kill@7 before_cursor (idempotent re-copy)", kill_at: Some(7), phase: "before_cursor" },
        Scenario { name: "kill@7 after_cursor (resume next)", kill_at: Some(7), phase: "after_cursor" },
        Scenario { name: "kill@15 (last) before_cursor", kill_at: Some(15), phase: "before_cursor" },
    ];

    let exe = std::env::current_exe().unwrap();
    let mut passed = 0;
    let mut failed = 0;

    for s in &scenarios {
        // dst 리셋 + checkpoint 제거 (깨끗한 시작).
        zero_fill(&dst, TOTAL_CHUNKS * CHUNK_BYTES as u64);
        let _ = fs::remove_file(&ckpt);

        // 1차 실행: kill-point 지정 시 자식이 abort 해야 한다.
        if let Some(k) = s.kill_at {
            let status = spawn_child(&exe, &src, &dst, &ckpt, Some(k), s.phase);
            if status.success() {
                println!("  [FAIL] {}: 자식이 abort 하지 않고 정상 종료함", s.name);
                failed += 1;
                continue;
            }
        }

        // 2차(또는 유일) 실행: kill 없이 → 복구/완주해야 한다.
        let status = spawn_child(&exe, &src, &dst, &ckpt, None, s.phase);
        if !status.success() {
            println!("  [FAIL] {}: 복구 실행이 실패 종료 ({status:?})", s.name);
            failed += 1;
            continue;
        }

        // 무결성: dst == src (byte-exact).
        let dst_sha = sha256_file(&dst);
        if dst_sha == src_sha {
            println!("  [PASS] {}", s.name);
            passed += 1;
        } else {
            println!("  [FAIL] {}: dst SHA 불일치 (복구 후 손상)", s.name);
            failed += 1;
        }
    }

    let _ = fs::remove_dir_all(&dir);
    println!("\nkill-test mock: {passed} passed, {failed} failed");
    i32::from(failed != 0)
}

fn spawn_child(
    exe: &Path,
    src: &Path,
    dst: &Path,
    ckpt: &Path,
    kill_at: Option<u64>,
    phase: &str,
) -> std::process::ExitStatus {
    let mut cmd = std::process::Command::new(exe);
    cmd.env("PARQ_KILLTEST_CHILD", "1")
        .env("KT_SRC", src)
        .env("KT_DST", dst)
        .env("KT_CKPT", ckpt)
        .env("KILL_PHASE", phase);
    if let Some(k) = kill_at {
        cmd.env("KILL_AT_CHUNK", k.to_string());
    } else {
        cmd.env_remove("KILL_AT_CHUNK");
    }
    cmd.status().expect("자식 프로세스 spawn 실패")
}

fn make_src(path: &Path) -> [u8; 32] {
    let mut f = File::create(path).unwrap();
    for i in 0..TOTAL_CHUNKS {
        // 청크 i 는 (i*7+13) 값으로 채움 → 잘못된 오프셋 복사 시 SHA 불일치로 검출.
        let val = ((i.wrapping_mul(7).wrapping_add(13)) & 0xFF) as u8;
        f.write_all(&vec![val; CHUNK_BYTES]).unwrap();
    }
    f.sync_all().unwrap();
    drop(f);
    sha256_file(path)
}

fn zero_fill(path: &Path, len: u64) {
    let f = File::create(path).unwrap();
    f.set_len(len).unwrap();
    f.sync_all().unwrap();
}

fn sha256_file(path: &Path) -> [u8; 32] {
    let mut f = File::open(path).unwrap();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    hasher.finalize().into()
}
