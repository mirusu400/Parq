// disk::enumerate() 의 read-only WMI 호출과 safety 가드 결과를 GUI 없이 검증하기 위한 도구.
//
//   cargo run --example enumerate
//
// 디스크/파티션 정보 + 각 디스크의 safety 검증 결과를 출력. 어떤 쓰기도 하지 않음.
// 내부 디스크에 대한 작업이 필요하면 PARQ_DEV_ALLOW_INTERNAL_DISKS=1 로 실행.

use parq_lib::{disk, safety};

fn main() {
    let disks = match disk::enumerate() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("disk::enumerate 실패: {e}");
            std::process::exit(1);
        }
    };

    println!("== {} disks ==\n", disks.len());
    for d in &disks {
        let writable = match safety::check_disk_writable(d) {
            Ok(()) => "WRITABLE".to_string(),
            Err(e) => format!("BLOCKED ({e})"),
        };
        println!(
            "Disk #{} {} ({:?}, {:?}) size={} GB system={} read_only={} removable={} → {writable}",
            d.number,
            d.model,
            d.bus_type,
            d.partition_style,
            d.size_bytes / 1_000_000_000,
            d.is_system,
            d.is_read_only,
            d.is_removable,
        );
        println!("  id={} serial={:?}", d.id, d.serial);
        for p in &d.partitions {
            println!(
                "  Part #{} offset={:>13} size={:>13} letter={:?} fs={:?} label={:?} boot={} sys={} hidden={} in_use={}",
                p.index,
                p.offset_bytes,
                p.size_bytes,
                p.drive_letter,
                p.file_system,
                p.label,
                p.is_boot,
                p.is_system,
                p.is_hidden,
                p.is_in_use,
            );
        }
        println!();
    }
}
