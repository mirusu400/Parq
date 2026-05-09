// Read-only Tauri commands. 디스크에 어떤 쓰기도 하지 않는다.

use tracing::{error, instrument};

use crate::disk::{self, Disk};
use crate::transaction::{self, TransactionLog};

/// 빌드된 Parq 버전 문자열을 반환한다. IPC smoke test 용도.
#[tauri::command]
#[instrument]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 저장된 모든 트랜잭션 로그 (`%LOCALAPPDATA%\Parq\transactions\*.json`) 를 반환한다.
/// **read-only**. 시작 시간 내림차순.
#[tauri::command]
#[instrument]
pub async fn list_transactions() -> Result<Vec<TransactionLog>, String> {
    tauri::async_runtime::spawn_blocking(transaction::list_logs)
        .await
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "list_transactions 워커 패닉");
            format!("list_transactions 작업이 비정상 종료되었습니다: {e}")
        })?
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "list_transactions 실패");
            e.to_string()
        })
}

/// 시스템에 연결된 모든 디스크/파티션을 열거한다. **read-only**.
///
/// WMI 쿼리는 블로킹이라 `spawn_blocking` 으로 워커 스레드로 보낸다 — Tauri 의 async 런타임을
/// 막지 않기 위함. 에러는 사용자에게 보여줄 한국어 문자열로 변환해 반환한다.
#[tauri::command]
#[instrument]
pub async fn list_disks() -> Result<Vec<Disk>, String> {
    tauri::async_runtime::spawn_blocking(disk::enumerate)
        .await
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "list_disks 워커 패닉");
            format!("디스크 열거 작업이 비정상 종료되었습니다: {e}")
        })?
        .map_err(|e| {
            error!(target: "parq::commands", error = %e, "list_disks 실패");
            e.to_string()
        })
}
