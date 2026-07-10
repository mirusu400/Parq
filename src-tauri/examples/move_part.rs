// move_engine::move_partition (완결된 파티션 이동: 데이터+테이블) 을 버려도 되는 VHD 에서 검증.
// ⚠ 파티션 데이터 + MBR 테이블을 바꾼다. 실제 데이터 디스크에 쓰지 말 것.
//
//   $env:PARQ_ENABLE_V2_DESTRUCTIVE=1; $env:PARQ_DEV_ALLOW_INTERNAL_DISKS=1
//   cargo run --example move_part -- <disk> <max_bytes> <src_start_lba> <new_start_lba> <ckpt>
//
// 안전장치: move_partition 내부의 알파게이트+디스크가드+파티션가드(부팅/시스템/마운트 거부)
//   + dst free 검증 + 인접 무변경 검증. 여기 추가로 크기 상한(max_bytes) 가드.

#[cfg(windows)]
fn main() {
    use std::path::PathBuf;

    use parq_lib::{move_engine, raw_io};

    let a: Vec<String> = std::env::args().collect();
    if a.len() < 6 {
        eprintln!("사용법: move_part <disk> <max_bytes> <src_start_lba> <new_start_lba> <ckpt>");
        std::process::exit(2);
    }
    let disk: u32 = a[1].parse().unwrap();
    let max_bytes: u64 = a[2].parse().unwrap();
    let src: u64 = a[3].parse().unwrap();
    let dst: u64 = a[4].parse().unwrap();
    let ckpt = PathBuf::from(&a[5]);

    // 크기 상한 가드 (read-only open 으로 확인).
    if let Ok(rd) = raw_io::open_physical_drive_readonly(disk) {
        if rd.geometry().total_bytes > max_bytes {
            eprintln!("가드 중단: 디스크 크기 {} > 상한 {max_bytes}", rd.geometry().total_bytes);
            std::process::exit(1);
        }
    }

    match move_engine::move_partition(disk, src, dst, &ckpt) {
        Ok(o) => {
            println!(
                "[PASS] 파티션 이동 완료: id={} {}→{} len={} sha256={} (resumed={})",
                o.partition_id, o.old_start_lba, o.new_start_lba, o.length_sectors, o.data.sha256, o.data.resumed
            );
        }
        Err(e) => {
            eprintln!("move_partition 실패: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("move_part 는 Windows 전용입니다.");
    std::process::exit(1);
}
