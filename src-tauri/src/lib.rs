use anyhow::Context as _;
use tracing::info;

pub mod commands;
pub mod disk;
pub mod error;
pub mod fs;
// V2 파티션 이동 엔진(Phase 3). raw_io write 위에 checkpoint/방향/라운드트립. Windows 전용.
#[cfg(windows)]
pub mod move_engine;
pub mod partition;
pub mod platform;
// V2 raw 디스크 I/O — read-only 파운데이션(Phase 2). Windows 전용. docs/v2-raw-io.md.
#[cfg(windows)]
pub mod raw_io;
pub mod safety;
pub mod transaction;

pub use error::{ParqError, Result};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> anyhow::Result<()> {
    init_tracing();
    info!(target: "parq::startup", "Parq starting");

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::read::version,
            commands::read::list_disks,
            commands::read::list_transactions,
            commands::write::plan_create_partition,
            commands::write::execute_create_partition_dangerous,
            commands::write::plan_set_label,
            commands::write::execute_set_label_dangerous,
            commands::write::plan_delete_partition,
            commands::write::execute_delete_partition_dangerous,
            commands::write::plan_dismount,
            commands::write::execute_dismount_dangerous,
            commands::write::get_resize_limits,
            commands::write::plan_resize_partition,
            commands::write::execute_resize_partition_dangerous,
            commands::v2::v2_destructive_enabled,
            commands::v2::plan_move_partition,
            commands::v2::execute_move_partition_dangerous,
        ])
        .run(tauri::generate_context!())
        .context("Tauri 애플리케이션 실행 실패")
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,parq=debug"));

    let _ = fmt().with_env_filter(filter).with_target(true).try_init();
}

#[cfg(test)]
pub(crate) mod test_support {
    //! 테스트 간 환경변수 / 글로벌 상태 접근을 직렬화하기 위한 공유 락.
    //! 같은 env 변수를 만지는 테스트가 여러 모듈에 분산되어 있으므로 crate 단위에서 관리한다.
    use std::sync::Mutex;
    pub static ENV_LOCK: Mutex<()> = Mutex::new(());
}
