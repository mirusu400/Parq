// move_engine 을 **버려도 되는 VHD** 위에서 검증하는 파괴적 도구.
// ⚠ 디스크 섹터를 이동(복사+덮어쓰기)한다. 실제 데이터 디스크에 쓰지 말 것.
//
//   $env:PARQ_ENABLE_V2_DESTRUCTIVE=1; $env:PARQ_DEV_ALLOW_INTERNAL_DISKS=1
//   cargo run --example move_region -- <disk> <max_bytes> <src_lba> <dst_lba> <len_sectors> <ckpt> [kill_at]
//
// 안전장치: open_writable 의 알파게이트+디스크가드 + 이 예제의 크기 상한(max_bytes) 가드.
// kill_at 지정 시: 그 청크까지 커서 기록 후 process::abort() → 전원 차단 모사. 재실행(kill_at 생략)
// 하면 checkpoint 에서 재개하고 라운드트립 검증까지 마쳐야 한다.

#[cfg(windows)]
fn main() {
    use std::path::PathBuf;

    use parq_lib::move_engine;

    let a: Vec<String> = std::env::args().collect();
    if a.len() < 7 {
        eprintln!("사용법: move_region <disk> <max_bytes> <src_lba> <dst_lba> <len_sectors> <ckpt> [kill_at]");
        std::process::exit(2);
    }
    let disk: u32 = a[1].parse().unwrap();
    let max_bytes: u64 = a[2].parse().unwrap();
    let src_lba: u64 = a[3].parse().unwrap();
    let dst_lba: u64 = a[4].parse().unwrap();
    let len: u64 = a[5].parse().unwrap();
    let ckpt = PathBuf::from(&a[6]);
    let kill_at: Option<u64> = a.get(7).and_then(|s| s.parse().ok());

    let plan = match move_engine::plan_move(disk, src_lba, dst_lba, len) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("plan_move 실패: {e}");
            std::process::exit(1);
        }
    };
    println!("plan: disk#{disk} src={src_lba} dst={dst_lba} len={len} dir={:?}", plan.direction);

    // 크기 상한 가드: open_writable 은 성공해도, 대상 디스크가 상한보다 크면 이동 자체를 막는다.
    // (plan_move 는 디스크를 안 열므로 여기서 geometry 확인 대신, execute 전에 read-only open 으로 검사)
    if let Ok(rd) = parq_lib::raw_io::open_physical_drive_readonly(disk) {
        if rd.geometry().total_bytes > max_bytes {
            eprintln!("가드 중단: 디스크 크기 {} > 상한 {max_bytes}", rd.geometry().total_bytes);
            std::process::exit(1);
        }
    }

    let progress = |done: u64, total: u64| {
        println!("  chunk {done}/{total}");
        if kill_at == Some(done) {
            eprintln!("  [KILL] chunk {done} 후 abort (전원 차단 모사)");
            std::process::abort();
        }
    };

    match move_engine::execute_move(&plan, &ckpt, progress) {
        Ok(o) => {
            println!(
                "[PASS] 이동 완료 (resumed={}) dir={:?} chunks={} sha256={}",
                o.resumed, o.direction, o.chunks, o.sha256
            );
        }
        Err(e) => {
            eprintln!("execute_move 실패: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("move_region 는 Windows 전용입니다.");
    std::process::exit(1);
}
