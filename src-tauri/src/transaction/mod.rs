// 트랜잭션 실행 감사 로그.
//
// 모든 파괴적 작업은 begin → run_step → commit | fail 흐름을 따른다.
// 로그는 `%LOCALAPPDATA%\Parq\transactions\<id>.json` 에 fsync 로 기록한다.
// 크래시 후 재시작 시 `result == None` 인 로그를 찾아 사용자에게 보고할 수 있다 (V2 에서 구현).
//
// 시간은 unix_nanos (u128) 로 저장 — chrono 등 새 의존성 없이 직렬화 가능. 사람이 읽을 ISO-8601
// 변환은 후속 작업에서 추가.
//
// 테스트 / 격리: PARQ_TRANSACTIONS_DIR 환경 변수로 로그 디렉토리를 override 할 수 있다.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{info, instrument, warn};

use crate::{ParqError, Result};

const TRANSACTIONS_DIR_ENV: &str = "PARQ_TRANSACTIONS_DIR";

static TXN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `begin` 호출에 필요한 메타데이터.
///
/// 디스크 객체 전체를 옮기는 대신 식별자만 받아 트랜잭션 모듈이 disk 모듈에 의존하지 않게
/// 한다. 호출자 (commands / partition / fs) 가 사람이 읽을 요약을 만들어 넘긴다.
#[derive(Debug, Clone)]
pub struct BeginParams<'a> {
    /// 작업 종류 — snake_case 권장. 예: `"create_partition"`, `"set_label"`.
    pub operation: &'a str,
    /// 대상 디스크의 안정적 식별자 (`Disk::id`).
    pub disk_id: &'a str,
    /// 디스크 요약 (모델, 크기, 버스 등). 로그를 사람이 읽을 때 필요.
    pub disk_summary: &'a str,
    /// 사용자에게 미리 보여줬던 plan 요약. 사후 감사용.
    pub plan_summary: &'a str,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    pub started_at_unix_nanos: u128,
    pub ended_at_unix_nanos: Option<u128>,
    pub status: StepStatus,
    /// 에러 메시지나 추가 메타. 성공 시 None.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionLog {
    pub id: String,
    pub started_at_unix_nanos: u128,
    pub operation: String,
    pub disk_id: String,
    pub disk_summary: String,
    pub plan_summary: String,
    pub steps: Vec<Step>,
    pub ended_at_unix_nanos: Option<u128>,
    /// 종료 결과. `"committed"` / `"failed: <reason>"` / `"dropped_without_finalize"` 등.
    pub result: Option<String>,
}

/// 진행 중인 트랜잭션 핸들.
///
/// `commit` 또는 `fail` 로 끝내야 한다. 둘 다 호출하지 않은 채 drop 되면 로그가
/// `dropped_without_finalize` 로 마감되어 디스크에 흔적이 남는다 (best-effort fsync).
pub struct Transaction {
    log_path: PathBuf,
    log: TransactionLog,
    finalized: bool,
}

impl Transaction {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.log.id
    }

    #[must_use]
    pub fn log_path(&self) -> &std::path::Path {
        &self.log_path
    }

    /// 단계 클로저를 실행하고 결과를 로그에 기록한다.
    ///
    /// 시작 시점과 종료 시점에 각각 fsync 한다 — 클로저 도중 크래시되면 `Running` 상태로
    /// 남고, 정상 종료 시 `Done` 또는 `Failed` 로 갱신된다.
    #[instrument(skip(self, f), fields(txn_id = %self.log.id, step = name))]
    pub fn run_step<F, T>(&mut self, name: &str, f: F) -> Result<T>
    where
        F: FnOnce() -> Result<T>,
    {
        let idx = self.log.steps.len();
        self.log.steps.push(Step {
            name: name.into(),
            started_at_unix_nanos: now_unix_nanos(),
            ended_at_unix_nanos: None,
            status: StepStatus::Running,
            detail: None,
        });
        self.write_to_disk()?;

        let result = f();

        let step = &mut self.log.steps[idx];
        step.ended_at_unix_nanos = Some(now_unix_nanos());
        match &result {
            Ok(_) => {
                step.status = StepStatus::Done;
            }
            Err(e) => {
                step.status = StepStatus::Failed;
                step.detail = Some(e.to_string());
            }
        }
        self.write_to_disk()?;
        result
    }

    /// 트랜잭션을 성공으로 마감.
    #[instrument(skip(self), fields(txn_id = %self.log.id))]
    pub fn commit(mut self) -> Result<()> {
        self.log.ended_at_unix_nanos = Some(now_unix_nanos());
        self.log.result = Some("committed".into());
        self.write_to_disk()?;
        self.finalized = true;
        info!(target: "parq::transaction", id = %self.log.id, "committed");
        Ok(())
    }

    /// 작업 실패를 기록하고 마감한다. 디스크 상태를 되돌리는 보상 작업은 수행하지 않는다.
    #[instrument(skip(self), fields(txn_id = %self.log.id))]
    pub fn fail(mut self, reason: &str) -> Result<()> {
        self.log.ended_at_unix_nanos = Some(now_unix_nanos());
        self.log.result = Some(format!("failed: {reason}"));
        self.write_to_disk()?;
        self.finalized = true;
        warn!(target: "parq::transaction", id = %self.log.id, %reason, "operation failed");
        Ok(())
    }

    /// 단순 직렬화 + atomic 교체 (tmp → rename) + fsync.
    fn write_to_disk(&self) -> Result<()> {
        let json = serde_json::to_vec_pretty(&self.log)
            .map_err(|e| ParqError::Transaction(format!("로그 직렬화 실패: {e}")))?;
        let tmp = self.log_path.with_extension("json.tmp");
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| {
                ParqError::Transaction(format!("로그 임시 파일 생성 실패 ({}): {e}", tmp.display()))
            })?;
        f.write_all(&json)
            .map_err(|e| ParqError::Transaction(format!("로그 쓰기 실패: {e}")))?;
        f.sync_all()
            .map_err(|e| ParqError::Transaction(format!("로그 fsync 실패: {e}")))?;
        drop(f);
        fs::rename(&tmp, &self.log_path).map_err(|e| {
            ParqError::Transaction(format!(
                "로그 rename 실패 ({} → {}): {e}",
                tmp.display(),
                self.log_path.display()
            ))
        })?;
        Ok(())
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if !self.finalized {
            self.log.ended_at_unix_nanos = Some(now_unix_nanos());
            self.log.result = Some("dropped_without_finalize".into());
            // best-effort — drop 경로에서 에러는 panic 화하지 않는다.
            if let Err(e) = self.write_to_disk() {
                warn!(
                    target: "parq::transaction",
                    id = %self.log.id,
                    error = %e,
                    "drop 시 로그 마감 실패"
                );
            } else {
                warn!(
                    target: "parq::transaction",
                    id = %self.log.id,
                    "commit/fail 없이 drop 됨 — 로그를 dropped_without_finalize 로 마감"
                );
            }
        }
    }
}

/// 저장된 모든 트랜잭션 로그를 읽어 반환한다 (`started_at` 내림차순). **read-only**.
///
/// 손상된 파일 / JSON 파싱 실패는 스킵하고 warn 로그만 남긴다 — 한 파일이 깨졌다고 전체
/// 목록 반환을 막지 않는다. 디렉토리 자체가 없으면 빈 배열을 반환.
#[instrument]
pub fn list_logs() -> Result<Vec<TransactionLog>> {
    let dir = transactions_dir()?;
    let mut logs = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(logs),
        Err(e) => {
            return Err(ParqError::Transaction(format!(
                "트랜잭션 디렉토리 읽기 실패 ({}): {e}",
                dir.display()
            )))
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        match fs::read_to_string(&path).and_then(|s| {
            serde_json::from_str::<TransactionLog>(&s)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        }) {
            Ok(log) => logs.push(log),
            Err(e) => {
                warn!(
                    target: "parq::transaction",
                    path = %path.display(),
                    error = %e,
                    "트랜잭션 로그 파싱 실패 — 스킵"
                );
            }
        }
    }
    logs.sort_by_key(|l| std::cmp::Reverse(l.started_at_unix_nanos));
    Ok(logs)
}

/// 새 트랜잭션 시작. 로그 파일을 생성하고 초기 메타데이터를 fsync 한다.
#[instrument(skip(params), fields(operation = params.operation, disk_id = params.disk_id))]
pub fn begin(params: BeginParams<'_>) -> Result<Transaction> {
    let dir = transactions_dir()?;
    fs::create_dir_all(&dir).map_err(|e| {
        ParqError::Transaction(format!("디렉토리 생성 실패 ({}): {e}", dir.display()))
    })?;

    let id = new_txn_id();
    let log_path = dir.join(format!("{id}.json"));
    let log = TransactionLog {
        id: id.clone(),
        started_at_unix_nanos: now_unix_nanos(),
        operation: params.operation.into(),
        disk_id: params.disk_id.into(),
        disk_summary: params.disk_summary.into(),
        plan_summary: params.plan_summary.into(),
        steps: Vec::new(),
        ended_at_unix_nanos: None,
        result: None,
    };
    let txn = Transaction {
        log_path,
        log,
        finalized: false,
    };
    txn.write_to_disk()?;
    info!(
        target: "parq::transaction",
        id = %txn.log.id,
        operation = %params.operation,
        disk_id = %params.disk_id,
        path = %txn.log_path.display(),
        "begin"
    );
    Ok(txn)
}

fn transactions_dir() -> Result<PathBuf> {
    if let Ok(override_path) = std::env::var(TRANSACTIONS_DIR_ENV) {
        return Ok(PathBuf::from(override_path));
    }
    #[cfg(windows)]
    {
        let local = std::env::var("LOCALAPPDATA").map_err(|_| {
            ParqError::Transaction("LOCALAPPDATA 환경변수가 설정되어 있지 않습니다".into())
        })?;
        Ok(PathBuf::from(local).join("Parq").join("transactions"))
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME")
            .map_err(|_| ParqError::Transaction("HOME 환경변수가 없습니다".into()))?;
        Ok(PathBuf::from(home).join(".parq").join("transactions"))
    }
}

fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// 프로세스 내 유일성 보장: nanos 가 같아도 atomic counter 와 PID 로 분리.
/// 외부 신뢰가 필요한 ID 가 아니므로 실제 UUID 는 사용하지 않는다.
fn new_txn_id() -> String {
    let nanos = now_unix_nanos();
    let pid = std::process::id();
    let seq = TXN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("txn-{nanos:x}-{pid:x}-{seq:x}")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// PARQ_TRANSACTIONS_DIR 를 만지는 테스트들 사이의 직렬화.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 테스트마다 새 임시 디렉토리로 PARQ_TRANSACTIONS_DIR 설정.
    /// 반환된 가드가 drop 될 때 env 를 정리하고 디렉토리도 지운다.
    struct TempTxnDir {
        path: PathBuf,
    }

    impl TempTxnDir {
        fn new(test_name: &str) -> Self {
            let path = std::env::temp_dir()
                .join("parq-test")
                .join(format!("{test_name}-{}", new_txn_id()));
            fs::create_dir_all(&path).expect("create temp dir");
            std::env::set_var(TRANSACTIONS_DIR_ENV, &path);
            Self { path }
        }
    }

    impl Drop for TempTxnDir {
        fn drop(&mut self) {
            std::env::remove_var(TRANSACTIONS_DIR_ENV);
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn read_log(path: &std::path::Path) -> TransactionLog {
        let raw = fs::read_to_string(path).expect("read log");
        serde_json::from_str(&raw).expect("parse log")
    }

    fn sample_params<'a>() -> BeginParams<'a> {
        BeginParams {
            operation: "create_partition",
            disk_id: "disk-test#1",
            disk_summary: "Test Disk 10GB NVMe",
            plan_summary: "create 1GB FAT32 at offset 1MiB",
        }
    }

    #[test]
    fn begin_writes_initial_log_and_returns_handle() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _dir = TempTxnDir::new("begin_writes");

        let txn = begin(sample_params()).expect("begin");
        let log_path = txn.log_path().to_path_buf();
        assert!(log_path.exists(), "로그 파일이 즉시 존재해야 함");

        let log = read_log(&log_path);
        assert_eq!(log.operation, "create_partition");
        assert_eq!(log.disk_id, "disk-test#1");
        assert!(log.steps.is_empty());
        assert!(log.ended_at_unix_nanos.is_none());
        assert!(log.result.is_none());

        // 명시적 commit 으로 drop 핸들러 경유하지 않게 한다.
        txn.commit().expect("commit");
    }

    #[test]
    fn run_step_records_running_then_done() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _dir = TempTxnDir::new("run_step_done");
        let mut txn = begin(sample_params()).expect("begin");
        let log_path = txn.log_path().to_path_buf();

        let result: Result<u32> = txn.run_step("dummy", || Ok(42));
        assert_eq!(result.expect("ok"), 42);

        let log = read_log(&log_path);
        assert_eq!(log.steps.len(), 1);
        assert_eq!(log.steps[0].name, "dummy");
        assert_eq!(log.steps[0].status, StepStatus::Done);
        assert!(log.steps[0].ended_at_unix_nanos.is_some());
        assert!(log.steps[0].detail.is_none());

        txn.commit().expect("commit");
    }

    #[test]
    fn run_step_records_failed_with_detail() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _dir = TempTxnDir::new("run_step_failed");
        let mut txn = begin(sample_params()).expect("begin");
        let log_path = txn.log_path().to_path_buf();

        let result: Result<()> = txn.run_step("explodes", || {
            Err(ParqError::ValidationFailed("의도적 실패".into()))
        });
        assert!(result.is_err());

        let log = read_log(&log_path);
        assert_eq!(log.steps.len(), 1);
        assert_eq!(log.steps[0].status, StepStatus::Failed);
        assert!(log.steps[0]
            .detail
            .as_ref()
            .expect("detail")
            .contains("의도적 실패"));

        txn.fail("의도적 실패").expect("fail");
    }

    #[test]
    fn commit_finalizes_log() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _dir = TempTxnDir::new("commit");
        let txn = begin(sample_params()).expect("begin");
        let log_path = txn.log_path().to_path_buf();

        txn.commit().expect("commit");

        let log = read_log(&log_path);
        assert_eq!(log.result.as_deref(), Some("committed"));
        assert!(log.ended_at_unix_nanos.is_some());
    }

    #[test]
    fn fail_finalizes_with_reason() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _dir = TempTxnDir::new("fail");
        let txn = begin(sample_params()).expect("begin");
        let log_path = txn.log_path().to_path_buf();

        txn.fail("디스크가 사라짐").expect("fail");

        let log = read_log(&log_path);
        let result = log.result.expect("result");
        assert!(result.starts_with("failed: "));
        assert!(result.contains("디스크가 사라짐"));
        assert!(log.ended_at_unix_nanos.is_some());
    }

    #[test]
    fn drop_without_finalize_records_dropped() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _dir = TempTxnDir::new("drop_without_finalize");

        let log_path = {
            let txn = begin(sample_params()).expect("begin");
            txn.log_path().to_path_buf()
            // txn drops here without commit/fail
        };

        let log = read_log(&log_path);
        assert_eq!(log.result.as_deref(), Some("dropped_without_finalize"));
        assert!(log.ended_at_unix_nanos.is_some());
    }

    #[test]
    fn list_logs_returns_empty_when_dir_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let path = std::env::temp_dir()
            .join("parq-test-missing")
            .join(new_txn_id());
        std::env::set_var(TRANSACTIONS_DIR_ENV, &path);
        let result = list_logs().expect("ok");
        std::env::remove_var(TRANSACTIONS_DIR_ENV);
        assert!(result.is_empty());
    }

    #[test]
    fn list_logs_returns_logs_sorted_newest_first() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _dir = TempTxnDir::new("list_logs_sorted");

        // 두 트랜잭션 생성 — 두 번째가 더 최근.
        let t1 = begin(sample_params()).expect("t1");
        t1.commit().expect("commit t1");
        // 살짝 대기는 불필요 — TXN_COUNTER + nanos 가 단조 증가 보장.
        let t2 = begin(BeginParams {
            operation: "set_label",
            ..sample_params()
        })
        .expect("t2");
        t2.commit().expect("commit t2");

        let logs = list_logs().expect("ok");
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].operation, "set_label");
        assert_eq!(logs[1].operation, "create_partition");
    }

    #[test]
    fn list_logs_skips_broken_files() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = TempTxnDir::new("list_logs_broken");

        // 정상 로그 하나
        let t = begin(sample_params()).expect("begin");
        t.commit().expect("commit");
        // 손상된 JSON 파일 하나 추가
        fs::write(dir.path.join("garbage.json"), "{not valid json").expect("write garbage");

        let logs = list_logs().expect("ok");
        assert_eq!(logs.len(), 1, "정상 로그만 반환되어야 함");
    }

    #[test]
    fn ids_are_unique_within_process() {
        let mut ids = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = new_txn_id();
            assert!(ids.insert(id), "id 중복 발생");
        }
    }
}
