#[cfg(windows)]
fn main() {
    let drive_letter = std::env::args().nth(1).unwrap_or_else(|| "C".to_string());
    match parq_lib::raw_io::volume::query_volume_extent(&drive_letter) {
        Ok(extent) => println!(
            "disk={} offset={} length={}",
            extent.disk_number, extent.starting_offset_bytes, extent.extent_length_bytes
        ),
        Err(error) => {
            eprintln!("volume extent query failed: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("volume_extent is Windows-only");
    std::process::exit(1);
}
