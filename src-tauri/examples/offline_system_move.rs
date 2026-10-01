#[cfg(windows)]
mod windows_main {
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    use parq_lib::disk::{self, BitLockerStatus, FileSystemKind, PartitionStyle};
    use parq_lib::{move_engine, raw_io, safety, ParqError, Result};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Request {
        request_version: u32,
        disk_number: u32,
        expected_disk_size: u64,
        expected_model: String,
        expected_serial: Option<String>,
        expected_source_size_before: u64,
        expected_source_size_after: u64,
        src_start_lba: u64,
        new_start_lba: u64,
        checkpoint_disk_number: u32,
        expected_checkpoint_disk_size: u64,
        expected_checkpoint_model: String,
        expected_checkpoint_serial: Option<String>,
        checkpoint_partition_start_lba: u64,
        expected_checkpoint_partition_size: u64,
        checkpoint_path: PathBuf,
        state_path: PathBuf,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Phase {
        Moving,
        PatchingNtfsBoot,
        Done,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct State {
        version: u32,
        request: Request,
        length_sectors: u64,
        phase: Phase,
    }

    fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
        let text = fs::read_to_string(path).map_err(|error| {
            ParqError::Platform(format!("{} 읽기 실패: {error}", path.display()))
        })?;
        serde_json::from_str(&text)
            .map_err(|error| ParqError::ValidationFailed(format!("JSON 파싱 실패: {error}")))
    }

    fn write_state(path: &Path, state: &State) -> Result<()> {
        let parent = path.parent().ok_or_else(|| {
            ParqError::ValidationFailed("state 경로에 부모 디렉터리가 없습니다".into())
        })?;
        fs::create_dir_all(parent)
            .map_err(|error| ParqError::Platform(format!("state 디렉터리 생성 실패: {error}")))?;
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|error| ParqError::Platform(format!("state 직렬화 실패: {error}")))?;
        let temporary = path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| ParqError::Platform(format!("state tmp 생성 실패: {error}")))?;
        file.write_all(&bytes)
            .map_err(|error| ParqError::Platform(format!("state 쓰기 실패: {error}")))?;
        file.sync_all()
            .map_err(|error| ParqError::Platform(format!("state fsync 실패: {error}")))?;
        drop(file);
        fs::rename(&temporary, path)
            .map_err(|error| ParqError::Platform(format!("state rename 실패: {error}")))?;
        Ok(())
    }

    fn append_run_log(state_path: &Path, message: &str) {
        let Some(parent) = state_path.parent() else {
            return;
        };
        let path = parent.join("run.log");
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{message}");
        }
    }

    fn drive_letter(path: &Path) -> Option<char> {
        let value = path.to_string_lossy();
        let bytes = value.as_bytes();
        (bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic())
            .then(|| (bytes[0] as char).to_ascii_uppercase())
    }

    fn expected_confirmation(request: &Request) -> String {
        let identity = request
            .expected_serial
            .as_deref()
            .filter(|serial| !serial.trim().is_empty())
            .unwrap_or(&request.expected_model);
        format!(
            "MOVE WINDOWS {identity} {} {}",
            request.src_start_lba, request.new_start_lba
        )
    }

    fn preflight_size_state(actual: u64, before: u64, after: u64) -> Result<&'static str> {
        if actual == after {
            Ok("ready")
        } else if actual == before {
            Ok("before_shrink")
        } else {
            Err(ParqError::ValidationFailed(format!(
                "preflight 원본 크기가 request의 축소 전/후 크기와 모두 다릅니다: actual={actual}, before={before}, after={after}"
            )))
        }
    }

    fn validate_disk_fingerprint(request: &Request) -> Result<(u64, disk::Disk)> {
        safety::require_offline_system_move()?;
        if request.request_version != 2 {
            return Err(ParqError::ValidationFailed(format!(
                "unsupported offline request version: {}",
                request.request_version
            )));
        }
        let confirmation = std::env::var("PARQ_OFFLINE_CONFIRMATION").unwrap_or_default();
        let expected = expected_confirmation(request);
        if confirmation != expected {
            return Err(ParqError::ValidationFailed(format!(
                "강한 확인 문구가 일치하지 않습니다. 필요한 값: {expected}"
            )));
        }

        let geometry = raw_io::open_physical_drive_readonly(request.disk_number)?.geometry();
        if geometry.total_bytes != request.expected_disk_size {
            return Err(ParqError::ValidationFailed(format!(
                "디스크 크기가 request와 다릅니다: actual={}, expected={}",
                geometry.total_bytes, request.expected_disk_size
            )));
        }
        let sector = u64::from(geometry.logical_sector_bytes);
        let disks = disk::enumerate()?;
        let target = disks
            .into_iter()
            .find(|disk| disk.number == request.disk_number)
            .ok_or_else(|| ParqError::DiskNotFound(format!("disk {}", request.disk_number)))?;
        if target.partition_style != PartitionStyle::Gpt
            || target.model != request.expected_model
            || target.serial != request.expected_serial
        {
            return Err(ParqError::ValidationFailed(
                "디스크 GPT/model/serial fingerprint가 request와 다릅니다".into(),
            ));
        }
        Ok((sector, target))
    }

    fn validate_checkpoint_partition(request: &Request, target_disk_number: u32) -> Result<()> {
        if request.checkpoint_disk_number == target_disk_number {
            return Err(ParqError::ValidationFailed(
                "checkpoint disk must be physically separate from the move target".into(),
            ));
        }
        let geometry =
            raw_io::open_physical_drive_readonly(request.checkpoint_disk_number)?.geometry();
        if geometry.total_bytes != request.expected_checkpoint_disk_size {
            return Err(ParqError::ValidationFailed(format!(
                "checkpoint disk size mismatch: actual={}, expected={}",
                geometry.total_bytes, request.expected_checkpoint_disk_size
            )));
        }
        let checkpoint_sector = u64::from(geometry.logical_sector_bytes);
        if checkpoint_sector == 0 {
            return Err(ParqError::Platform(
                "checkpoint disk logical sector size is zero".into(),
            ));
        }
        let checkpoint_disk = disk::enumerate()?
            .into_iter()
            .find(|disk| disk.number == request.checkpoint_disk_number)
            .ok_or_else(|| {
                ParqError::DiskNotFound(format!("disk {}", request.checkpoint_disk_number))
            })?;
        if checkpoint_disk.partition_style != PartitionStyle::Gpt
            || checkpoint_disk.is_read_only
            || checkpoint_disk.is_system
            || checkpoint_disk.model != request.expected_checkpoint_model
            || checkpoint_disk.serial != request.expected_checkpoint_serial
        {
            return Err(ParqError::ValidationFailed(
                "checkpoint disk GPT/writable/system/model/serial fingerprint mismatch".into(),
            ));
        }
        let checkpoint_partition = checkpoint_disk
            .partitions
            .iter()
            .find(|partition| {
                partition.offset_bytes / checkpoint_sector == request.checkpoint_partition_start_lba
            })
            .ok_or_else(|| {
                ParqError::ValidationFailed("checkpoint 파티션을 찾을 수 없습니다".into())
            })?;
        if checkpoint_partition.size_bytes != request.expected_checkpoint_partition_size
            || checkpoint_partition.is_boot
            || checkpoint_partition.is_system
            || checkpoint_partition.bitlocker_status != BitLockerStatus::NotEncrypted
            || !matches!(
                checkpoint_partition.file_system,
                FileSystemKind::Ntfs | FileSystemKind::Fat32 | FileSystemKind::ExFat
            )
        {
            return Err(ParqError::ValidationFailed(
                "checkpoint partition size/role/filesystem/encryption fingerprint mismatch".into(),
            ));
        }
        let checkpoint_letter = drive_letter(&request.checkpoint_path);
        if checkpoint_letter.is_none() || checkpoint_letter != drive_letter(&request.state_path) {
            return Err(ParqError::ValidationFailed(
                "checkpoint and state paths must use the same drive".into(),
            ));
        }
        let checkpoint_letter = checkpoint_letter.expect("checked above");
        let extent = raw_io::volume::query_volume_extent(&checkpoint_letter.to_string())?;
        if extent.disk_number != request.checkpoint_disk_number
            || extent.starting_offset_bytes / checkpoint_sector
                != request.checkpoint_partition_start_lba
            || extent.extent_length_bytes != request.expected_checkpoint_partition_size
        {
            return Err(ParqError::ValidationFailed(format!(
                "checkpoint volume extent mismatch: disk={}, start={}, length={}",
                extent.disk_number,
                extent.starting_offset_bytes / checkpoint_sector,
                extent.extent_length_bytes
            )));
        }
        Ok(())
    }

    fn preflight(request: &Request) -> Result<()> {
        let (sector, target) = validate_disk_fingerprint(request)?;
        validate_checkpoint_partition(request, target.number)?;
        let source = target
            .partitions
            .iter()
            .find(|partition| partition.offset_bytes / sector == request.src_start_lba)
            .ok_or_else(|| {
                ParqError::ValidationFailed("preflight 원본 파티션을 찾을 수 없습니다".into())
            })?;
        let size_state = preflight_size_state(
            source.size_bytes,
            request.expected_source_size_before,
            request.expected_source_size_after,
        )?;
        if source.file_system != FileSystemKind::Ntfs
            || source.bitlocker_status != BitLockerStatus::NotEncrypted
            || source.is_system
        {
            return Err(ParqError::ValidationFailed(format!(
                "preflight 원본 파티션 fingerprint 불일치: size={}, fs={:?}, bitlocker={:?}, system={}",
                source.size_bytes,
                source.file_system,
                source.bitlocker_status,
                source.is_system
            )));
        }
        println!(
            "[PASS] preflight: disk={} model={} source={} size={} state={size_state}",
            request.disk_number, target.model, request.src_start_lba, source.size_bytes
        );
        Ok(())
    }

    fn run(request_path: &Path) -> Result<()> {
        let request: Request = read_json(request_path)?;
        let (sector, target) = validate_disk_fingerprint(&request)?;
        validate_checkpoint_partition(&request, target.number)?;
        let source_drive_letter = target
            .partitions
            .iter()
            .find(|partition| {
                let start_lba = partition.offset_bytes / sector;
                start_lba == request.src_start_lba || start_lba == request.new_start_lba
            })
            .and_then(|partition| partition.drive_letter.as_deref())
            .ok_or_else(|| {
                ParqError::ValidationFailed(
                    "source partition must have a drive letter before locking".into(),
                )
            })?
            .to_string();
        let mut state = if request.state_path.exists() {
            let state: State = read_json(&request.state_path)?;
            if state.version != 2 || state.request != request {
                return Err(ParqError::ValidationFailed(
                    "기존 offline state가 현재 request와 일치하지 않습니다".into(),
                ));
            }
            state
        } else {
            let source = target
                .partitions
                .iter()
                .find(|partition| partition.offset_bytes / sector == request.src_start_lba)
                .ok_or_else(|| {
                    ParqError::ValidationFailed("request의 원본 파티션을 찾을 수 없습니다".into())
                })?;
            safety::check_partition_offline_system_move_lockable(&target, source)?;
            if !matches!(
                source.file_system,
                FileSystemKind::Ntfs | FileSystemKind::Unknown
            ) {
                return Err(ParqError::ValidationFailed(
                    "원본 파티션이 NTFS가 아닙니다".into(),
                ));
            }
            if source.size_bytes != request.expected_source_size_after {
                return Err(ParqError::ValidationFailed(format!(
                    "shrink 후 원본 크기가 request와 다릅니다: actual={}, expected={}",
                    source.size_bytes, request.expected_source_size_after
                )));
            }
            let length_sectors = source.size_bytes / sector;
            let state = State {
                version: 2,
                request: request.clone(),
                length_sectors,
                phase: Phase::Moving,
            };
            write_state(&request.state_path, &state)?;
            state
        };

        match state.phase {
            Phase::Moving => {
                let outcome = move_engine::move_partition_offline_system(
                    request.disk_number,
                    request.src_start_lba,
                    request.new_start_lba,
                    &source_drive_letter,
                    &request.checkpoint_path,
                    |event| {
                        if let move_engine::MoveEvent::CheckpointPersisted { chunk, total } = event
                        {
                            if chunk == total || chunk % 256 == 0 {
                                let percent = chunk.saturating_mul(100) / total.max(1);
                                let message = format!(
                                    "[MOVE] {chunk}/{total} chunks ({percent}%) checkpointed"
                                );
                                println!("{message}");
                                append_run_log(&request.state_path, &message);
                            }
                        }
                    },
                )?;
                if outcome.length_sectors != state.length_sectors {
                    return Err(ParqError::ValidationFailed(
                        "이동 결과 길이가 offline state와 다릅니다".into(),
                    ));
                }
                state.phase = Phase::Done;
                write_state(&request.state_path, &state)?;
            }
            Phase::PatchingNtfsBoot => {
                move_engine::patch_ntfs_boot_metadata_offline(
                    request.disk_number,
                    request.src_start_lba,
                    request.new_start_lba,
                    state.length_sectors,
                    &source_drive_letter,
                )?;
                state.phase = Phase::Done;
                write_state(&request.state_path, &state)?;
            }
            Phase::Done => {}
        }
        println!(
            "[PASS] offline Windows partition move complete: disk={} {}→{} len={}",
            request.disk_number, request.src_start_lba, request.new_start_lba, state.length_sectors
        );
        Ok(())
    }

    pub fn main() {
        let mut args = std::env::args_os();
        let _program = args.next();
        let Some(first) = args.next() else {
            eprintln!("사용법: offline_system_move [--preflight] <request.json>");
            std::process::exit(2);
        };
        let (preflight_only, request_path) = if first == "--preflight" {
            let Some(path) = args.next() else {
                eprintln!("사용법: offline_system_move [--preflight] <request.json>");
                std::process::exit(2);
            };
            (true, path)
        } else {
            (false, first)
        };
        if args.next().is_some() {
            eprintln!("사용법: offline_system_move [--preflight] <request.json>");
            std::process::exit(2);
        }
        let result = if preflight_only {
            read_json(Path::new(&request_path)).and_then(|request| preflight(&request))
        } else {
            let request_path = Path::new(&request_path);
            let result = run(request_path);
            if let Err(error) = &result {
                if let Ok(request) = read_json::<Request>(request_path) {
                    append_run_log(&request.state_path, &format!("ERROR: {error}"));
                }
            }
            result
        };
        if let Err(error) = result {
            eprintln!("offline system move 실패: {error}");
            std::process::exit(1);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::preflight_size_state;

        #[test]
        fn preflight_accepts_before_and_after_sizes_only() {
            assert_eq!(preflight_size_state(100, 100, 80).unwrap(), "before_shrink");
            assert_eq!(preflight_size_state(80, 100, 80).unwrap(), "ready");
            assert_eq!(preflight_size_state(100, 100, 100).unwrap(), "ready");
            assert!(preflight_size_state(90, 100, 80).is_err());
        }
    }
}

#[cfg(windows)]
fn main() {
    windows_main::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("offline_system_move는 Windows 전용입니다");
    std::process::exit(1);
}
