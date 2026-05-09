// 파티션 생성 4단계 (plan → validate → preview → execute) 의 CLI 테스트 하네스.
// frontend UI 작업 전 backend 만으로 Disk #1 같은 테스트 디스크에서 검증하기 위한 도구.
//
// 사용법:
//   PARQ_DEV_ALLOW_INTERNAL_DISKS=1 \
//     cargo run --example create_partition -- <DiskNumber> <Size> <FS> [Label]
//
//   <Size> = "max" 또는 바이트 수
//   <FS>   = FAT32 | exFAT | NTFS
//
// 실행 직전에 디스크 모델명을 타이핑해서 확인해야 진행 — safety-model.md 의 "preview 단계
// 명시적 확인" 정책을 CLI 에서도 강제한다.

use std::io::{self, BufRead, Write};

use parq_lib::disk::{self, FileSystemKind};
use parq_lib::partition::{self, SizeRequest};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 || args.len() > 5 {
        eprintln!(
            "사용법: {} <DiskNumber> <max|bytes> <FAT32|exFAT|NTFS> [Label]",
            args.first().map(String::as_str).unwrap_or("create_partition")
        );
        std::process::exit(2);
    }

    let disk_number: u32 = match args[1].parse() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("DiskNumber 파싱 실패: {e}");
            std::process::exit(2);
        }
    };

    let size_request = parse_size(&args[2]).unwrap_or_else(|e| {
        eprintln!("크기 파싱 실패: {e}");
        std::process::exit(2);
    });

    let file_system = parse_fs(&args[3]).unwrap_or_else(|e| {
        eprintln!("파일시스템 파싱 실패: {e}");
        std::process::exit(2);
    });

    let label = args.get(4).cloned();

    // Step 1: 디스크 enumerate, 대상 찾기
    let disks = match disk::enumerate() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("disk::enumerate 실패: {e}");
            std::process::exit(1);
        }
    };
    let Some(target) = disks.into_iter().find(|d| d.number == disk_number) else {
        eprintln!("디스크 번호 {disk_number} 을 찾을 수 없습니다");
        std::process::exit(1);
    };

    // Step 2 & 3: plan 생성 (safety guard 포함, read-only)
    let plan = match partition::plan_create_partition(&target, size_request, file_system, label) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("plan 거부됨: {e}");
            std::process::exit(1);
        }
    };

    // Preview
    println!();
    println!("=== plan ===");
    println!("operation       : create_partition");
    println!(
        "disk            : #{} {} ({}, {:?}, {:?})",
        plan.disk.number,
        plan.disk.model,
        format_bytes(plan.disk.size_bytes),
        plan.disk.bus_type,
        plan.disk.partition_style,
    );
    println!("size            : {:?}", plan.size_request);
    println!("file system     : {:?}", plan.file_system);
    println!(
        "label           : {}",
        plan.label.as_deref().unwrap_or("(없음)")
    );
    println!("initialize GPT  : {}", plan.initialize_as_gpt);
    println!("summary         : {}", plan.summary);
    println!("============");
    println!();
    print!(
        "이 작업을 실행하려면 디스크 모델명을 정확히 입력하세요\n(\"{}\"): ",
        plan.disk.model
    );
    let _ = io::stdout().flush();

    let mut buf = String::new();
    let stdin = io::stdin();
    if stdin.lock().read_line(&mut buf).is_err() {
        eprintln!("입력을 읽지 못했습니다");
        std::process::exit(1);
    }
    let typed = buf.trim();
    if typed != plan.disk.model {
        eprintln!("모델명 불일치 — 취소합니다 (입력: {typed:?})");
        std::process::exit(1);
    }

    // Step 4: execute
    println!("실행 중...");
    if let Err(e) = partition::execute_create_partition(plan) {
        eprintln!("실행 실패: {e}");
        std::process::exit(1);
    }
    println!("완료. 트랜잭션 로그: %LOCALAPPDATA%\\Parq\\transactions\\");
}

fn parse_size(s: &str) -> Result<SizeRequest, String> {
    if s.eq_ignore_ascii_case("max") {
        return Ok(SizeRequest::UseMaximum);
    }
    s.parse::<u64>()
        .map(SizeRequest::Bytes)
        .map_err(|e| format!("{e}"))
}

fn parse_fs(s: &str) -> Result<FileSystemKind, String> {
    match s.to_ascii_uppercase().as_str() {
        "FAT32" => Ok(FileSystemKind::Fat32),
        "EXFAT" => Ok(FileSystemKind::ExFat),
        "NTFS" => Ok(FileSystemKind::Ntfs),
        other => Err(format!("지원하지 않는 파일시스템: {other}")),
    }
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
