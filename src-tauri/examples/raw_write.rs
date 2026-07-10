// raw_io::write 프리미티브를 **버려도 되는 VHD** 위에서 검증하는 파괴적 도구.
// ⚠ 이 도구는 디스크 섹터를 덮어쓴다. 절대 실제 데이터 디스크에 쓰지 말 것.
//
//   $env:PARQ_ENABLE_V2_DESTRUCTIVE=1
//   $env:PARQ_DEV_ALLOW_INTERNAL_DISKS=1   # VHD 는 내부 버스라 필요
//   cargo run --example raw_write -- <disk_number> <max_size_bytes> [start_lba] [length_sectors]
//
// 안전장치 (다층):
//   - raw_io::write::open_writable 가 알파 게이트 + check_disk_writable(시스템 디스크 차단) 강제.
//   - 이 example 은 추가로 **크기 상한 가드**: 대상 디스크 total_bytes 가 <max_size_bytes> 이하가
//     아니면 즉시 중단. 작은 테스트 VHD(예: 64 MiB) 만 통과 → 10GB/100GB 실디스크는 물리적으로
//     대상이 될 수 없다. 디스크 번호를 잘못 넣어도 크기가 안 맞으면 write 안 됨.
//   - 쓰기 후 되읽어 SHA256 라운드트립까지 확인.
//
// 패턴: 섹터마다 앞 8B = 절대 LBA(LE), 나머지 = (LBA & 0xFF) ^ 0x5A. (vhd-matrix.ps1 과 유사)

#[cfg(windows)]
fn main() {
    use parq_lib::raw_io;
    use sha2::{Digest, Sha256};

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("사용법: cargo run --example raw_write -- <disk_number> <max_size_bytes> [start_lba] [length_sectors]");
        std::process::exit(2);
    }
    let number: u32 = args[1].parse().unwrap_or_else(|_| {
        eprintln!("disk_number 파싱 실패");
        std::process::exit(2);
    });
    let max_size_bytes: u64 = args[2].parse().unwrap_or_else(|_| {
        eprintln!("max_size_bytes 파싱 실패");
        std::process::exit(2);
    });
    let start_lba: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
    let length_sectors: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(64);

    // 쓰기 가능 핸들 (알파 게이트 + 디스크 가드는 open_writable 내부에서 강제).
    let disk = match raw_io::write::open_writable(number) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("open_writable 실패: {e}");
            std::process::exit(1);
        }
    };

    let geo = disk.geometry();
    // 크기 상한 가드 — 이 example 만의 추가 방어선.
    if geo.total_bytes > max_size_bytes {
        eprintln!(
            "가드 중단: 대상 디스크 크기 {} > 상한 {max_size_bytes}. 작은 테스트 VHD 만 허용됩니다.",
            geo.total_bytes
        );
        std::process::exit(1);
    }

    let sector = geo.logical_sector_bytes as usize;
    if start_lba + length_sectors > geo.sector_count() {
        eprintln!("범위 초과: 디스크 sector_count={}", geo.sector_count());
        std::process::exit(1);
    }

    // 결정론적 패턴 생성.
    let mut data = vec![0u8; length_sectors as usize * sector];
    for s in 0..length_sectors {
        let abs_lba = start_lba + s;
        let base = s as usize * sector;
        data[base..base + 8].copy_from_slice(&abs_lba.to_le_bytes());
        let fill = ((abs_lba & 0xFF) as u8) ^ 0x5A;
        for b in &mut data[base + 8..base + sector] {
            *b = fill;
        }
    }
    let expected: [u8; 32] = Sha256::digest(&data).into();

    println!("== raw_write (PARTIALLY DESTRUCTIVE) PhysicalDrive{number} ==");
    println!("  total_bytes = {} (상한 {max_size_bytes})", geo.total_bytes);
    println!("  write LBA {start_lba}..{} ({length_sectors} sectors)", start_lba + length_sectors);

    if let Err(e) = disk.write_sectors(start_lba, &data) {
        eprintln!("write_sectors 실패: {e}");
        std::process::exit(1);
    }
    println!("  write 완료. expected sha256 = {}", hex(&expected));

    // 되읽기 라운드트립 (read-only 경로).
    drop(disk);
    let rdisk = match raw_io::open_physical_drive_readonly(number) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("되읽기 open 실패: {e}");
            std::process::exit(1);
        }
    };
    let mut hasher = Sha256::new();
    if let Err(e) = raw_io::for_each_chunk(&rdisk, start_lba, length_sectors, |c| {
        hasher.update(c);
        Ok(())
    }) {
        eprintln!("되읽기 실패: {e}");
        std::process::exit(1);
    }
    let actual: [u8; 32] = hasher.finalize().into();
    println!("  readback sha256 = {}", hex(&actual));

    if actual == expected {
        println!("\n[PASS] write→read SHA256 라운드트립 일치. raw write 프리미티브 검증 완료.");
    } else {
        eprintln!("\n[FAIL] 라운드트립 불일치 — write 가 올바르지 않음.");
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("raw_write 는 Windows 전용입니다.");
    std::process::exit(1);
}
