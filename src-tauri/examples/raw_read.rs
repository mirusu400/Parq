// raw_io 의 read-only 파운데이션(open + geometry + 섹터 read)을 GUI 없이 검증하는 도구.
// **읽기 전용.** 어떤 write 도 하지 않는다. docs/v2-raw-io.md §7 (PR3 게이트) 자가 검증용.
//
//   cargo run --example raw_read -- <disk_number> [start_lba] [length_sectors]
//
// 예:
//   cargo run --example raw_read -- 2                # 디스크 2, 앞 32 섹터
//   cargo run --example raw_read -- 2 2048 4096      # 디스크 2, LBA 2048 부터 4096 섹터
//
// 관리자 권한 PowerShell 에서 실행해야 한다(raw 핸들). 대상 디스크 번호는 Get-Disk 로 확인.
// ※ read-only 라 데이터 위험은 없지만, 디스크 번호는 실행마다 바뀔 수 있으니 매번 확인할 것.
//
// SHA256 라운드트립: 이 example 이 출력하는 sha256 값은 같은 구간에 대한
// `scripts/hash-region.ps1 -DiskNumber N -StartLba L -LengthSectors C` 값과 **일치해야 한다**
// (docs/v2-raw-io.md §7, PR3 게이트). 해시는 for_each_chunk 로 스트리밍해 계산 — raw_io 자체는
// 여전히 해시 의존성이 없다(sha2 는 dev-dependency, example/test 전용).

#[cfg(windows)]
fn main() {
    use parq_lib::raw_io;
    use sha2::{Digest, Sha256};

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "사용법: cargo run --example raw_read -- <disk_number> [start_lba] [length_sectors]"
        );
        std::process::exit(2);
    }
    let number: u32 = match args[1].parse() {
        Ok(n) => n,
        Err(_) => {
            eprintln!("disk_number 파싱 실패: {:?}", args[1]);
            std::process::exit(2);
        }
    };
    let start_lba: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let length_sectors: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(32);

    let disk = match raw_io::open_physical_drive_readonly(number) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("open 실패: {e}");
            std::process::exit(1);
        }
    };

    let geo = disk.geometry();
    println!("== PhysicalDrive{number} (read-only) ==");
    println!("  logical_sector_bytes = {}", geo.logical_sector_bytes);
    println!("  total_bytes          = {}", geo.total_bytes);
    println!("  sector_count         = {}", geo.sector_count());
    println!(
        "  read range           = LBA {}..{} ({} sectors)\n",
        start_lba,
        start_lba + length_sectors,
        length_sectors
    );

    let mut hasher = Sha256::new();
    let mut bytes: u64 = 0;
    let mut first_chunk_dumped = false;

    let result = raw_io::for_each_chunk(&disk, start_lba, length_sectors, |chunk| {
        if !first_chunk_dumped {
            print!("  first 64 bytes:");
            for (i, b) in chunk.iter().take(64).enumerate() {
                if i % 16 == 0 {
                    print!("\n    ");
                }
                print!("{b:02x} ");
            }
            println!("\n");
            first_chunk_dumped = true;
        }
        hasher.update(chunk);
        bytes += chunk.len() as u64;
        Ok(())
    });

    match result {
        Ok(()) => {
            let digest = hasher.finalize();
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            println!("  bytes_read = {bytes}");
            println!("  sha256     = {hex}");
            println!(
                "\n대조: scripts/hash-region.ps1 -DiskNumber {number} -StartLba {start_lba} -LengthSectors {length_sectors}"
            );
            println!("read-only 검증 완료. 디스크에 어떤 write 도 하지 않았습니다.");
        }
        Err(e) => {
            eprintln!("read 실패: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("raw_read 는 Windows 전용입니다.");
    std::process::exit(1);
}
