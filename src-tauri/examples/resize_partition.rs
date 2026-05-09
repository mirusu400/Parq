// NTFS 파티션 리사이즈 4단계의 CLI 테스트 하네스.
//
//   PARQ_DEV_ALLOW_INTERNAL_DISKS=1 \
//     cargo run --example resize_partition -- <DiskNumber> <PartitionIndex> <NewSizeBytes>
//
// V1 은 NTFS 만 지원 (Resize-Partition 한계). 시작 LBA 는 고정 — 끝 경계만 이동한다.
// shrink 시 immovable 파일 (MFT, 페이지파일 등) 이 min 한계를 끌어올린다.

use std::io::{self, BufRead, Write};

use parq_lib::disk;
use parq_lib::partition;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!(
            "사용법: {} <DiskNumber> <PartitionIndex> <NewSizeBytes>",
            args.first().map(String::as_str).unwrap_or("resize_partition")
        );
        std::process::exit(2);
    }

    let disk_number: u32 = args[1].parse().unwrap_or_else(|e| {
        eprintln!("DiskNumber 파싱 실패: {e}");
        std::process::exit(2);
    });
    let partition_index: u32 = args[2].parse().unwrap_or_else(|e| {
        eprintln!("PartitionIndex 파싱 실패: {e}");
        std::process::exit(2);
    });
    let new_size_bytes: u64 = args[3].parse().unwrap_or_else(|e| {
        eprintln!("NewSizeBytes 파싱 실패: {e}");
        std::process::exit(2);
    });

    let disks = disk::enumerate().unwrap_or_else(|e| {
        eprintln!("disk::enumerate 실패: {e}");
        std::process::exit(1);
    });
    let Some(target_disk) = disks.into_iter().find(|d| d.number == disk_number) else {
        eprintln!("디스크 번호 {disk_number} 을 찾을 수 없습니다");
        std::process::exit(1);
    };
    let Some(target_partition) = target_disk
        .partitions
        .iter()
        .find(|p| p.index == partition_index)
    else {
        eprintln!(
            "디스크 #{} 에서 파티션 #{} 을 찾을 수 없습니다",
            disk_number, partition_index
        );
        std::process::exit(1);
    };
    let partition_id = target_partition.id.clone();

    // 사용자에게 limits 먼저 보여주기 — plan 거부 전에 가능한 범위를 알 수 있게.
    let limits = match partition::query_resize_limits(&target_disk, &partition_id) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("리사이즈 한계 조회 실패: {e}");
            std::process::exit(1);
        }
    };
    println!();
    println!("=== resize limits ===");
    println!("current : {}", format_bytes(limits.current_bytes));
    println!("min     : {}", format_bytes(limits.min_bytes));
    println!("max     : {}", format_bytes(limits.max_bytes));
    println!("====================");

    let plan = match partition::plan_resize_partition(&target_disk, &partition_id, new_size_bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("plan 거부됨: {e}");
            std::process::exit(1);
        }
    };

    println!();
    println!("=== plan ===");
    println!("operation       : resize_partition");
    println!(
        "disk            : #{} {}",
        plan.disk.number, plan.disk.model
    );
    println!(
        "partition       : #{} ({:?}, label=\"{}\")",
        plan.partition.index,
        plan.partition.file_system,
        plan.partition.label.as_deref().unwrap_or(""),
    );
    println!(
        "size            : {} → {}",
        format_bytes(plan.current_size_bytes),
        format_bytes(plan.new_size_bytes),
    );
    println!("summary         : {}", plan.summary);
    println!("============");
    println!();
    if plan.new_size_bytes < plan.current_size_bytes {
        println!("⚠ shrink — 끝쪽 데이터가 잘립니다. 백업 확인하세요.");
    } else {
        println!("ℹ extend — 뒤쪽 미할당 영역을 흡수합니다.");
    }
    print!(
        "확인하려면 디스크 모델명을 정확히 입력하세요\n(\"{}\"): ",
        plan.disk.model
    );
    let _ = io::stdout().flush();

    let mut buf = String::new();
    if io::stdin().lock().read_line(&mut buf).is_err() {
        eprintln!("입력을 읽지 못했습니다");
        std::process::exit(1);
    }
    let typed = buf.trim();
    if typed != plan.disk.model {
        eprintln!("모델명 불일치 — 취소합니다 (입력: {typed:?})");
        std::process::exit(1);
    }

    println!("실행 중...");
    if let Err(e) = partition::execute_resize_partition(plan) {
        eprintln!("실행 실패: {e}");
        std::process::exit(1);
    }
    println!("완료. 트랜잭션 로그: %LOCALAPPDATA%\\Parq\\transactions\\");
}

fn format_bytes(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = b as f64;
    let mut idx = 0;
    while value >= 1000.0 && idx + 1 < UNITS.len() {
        value /= 1000.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{b} B")
    } else {
        format!("{value:.1} {}", UNITS[idx])
    }
}
