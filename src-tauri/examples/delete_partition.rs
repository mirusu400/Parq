// 파티션 삭제 4단계의 CLI 테스트 하네스.
//
//   PARQ_DEV_ALLOW_INTERNAL_DISKS=1 \
//     cargo run --example delete_partition -- <DiskNumber> <PartitionIndex>
//
// 마운트된 파티션은 safety::check_partition_destructive 가 거부한다 — 먼저 드라이브 문자를
// 제거해야 한다 (`Remove-PartitionAccessPath` 또는 Windows 디스크 관리).

use std::io::{self, BufRead, Write};

use parq_lib::disk;
use parq_lib::partition;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!(
            "사용법: {} <DiskNumber> <PartitionIndex>",
            args.first().map(String::as_str).unwrap_or("delete_partition")
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

    let plan = match partition::plan_delete_partition(&target_disk, &partition_id) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("plan 거부됨: {e}");
            std::process::exit(1);
        }
    };

    println!();
    println!("=== plan ===");
    println!("operation       : delete_partition");
    println!(
        "disk            : #{} {}",
        plan.disk.number, plan.disk.model
    );
    println!(
        "partition       : #{} ({}, {:?}, label=\"{}\")",
        plan.partition.index,
        format_bytes(plan.partition.size_bytes),
        plan.partition.file_system,
        plan.partition.label.as_deref().unwrap_or(""),
    );
    println!("summary         : {}", plan.summary);
    println!("============");
    println!();
    println!("⚠ 이 작업은 데이터를 영구히 파괴합니다. 되돌릴 수 없습니다.");
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
    if let Err(e) = partition::execute_delete_partition(plan) {
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
