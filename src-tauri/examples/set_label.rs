// 라벨 변경 4단계 패턴의 CLI 테스트 하네스.
//
//   PARQ_DEV_ALLOW_INTERNAL_DISKS=1 \
//     cargo run --example set_label -- <DiskNumber> <PartitionIndex> <NewLabel>
//
// 디스크 모델명 타이핑 확인 후 실행 — preview 강제는 create_partition 과 동일 패턴.

use std::io::{self, BufRead, Write};

use parq_lib::disk;
use parq_lib::partition;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!(
            "사용법: {} <DiskNumber> <PartitionIndex> <NewLabel>",
            args.first().map(String::as_str).unwrap_or("set_label")
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
    let new_label = args[3].clone();

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

    let plan = match partition::plan_set_label(&target_disk, &partition_id, new_label) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("plan 거부됨: {e}");
            std::process::exit(1);
        }
    };

    println!();
    println!("=== plan ===");
    println!("operation       : set_label");
    println!(
        "disk            : #{} {}",
        plan.disk.number, plan.disk.model
    );
    println!(
        "partition       : #{} ({}: {} → {})",
        plan.partition.index,
        plan.partition.drive_letter.as_deref().unwrap_or(""),
        plan.partition.label.as_deref().unwrap_or("(없음)"),
        plan.new_label,
    );
    println!("file system     : {:?}", plan.partition.file_system);
    println!("summary         : {}", plan.summary);
    println!("============");
    println!();
    print!(
        "이 작업을 실행하려면 디스크 모델명을 정확히 입력하세요\n(\"{}\"): ",
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
    if let Err(e) = partition::execute_set_label(plan) {
        eprintln!("실행 실패: {e}");
        std::process::exit(1);
    }
    println!("완료. 트랜잭션 로그: %LOCALAPPDATA%\\Parq\\transactions\\");
}
