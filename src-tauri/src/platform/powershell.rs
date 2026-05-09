// PowerShell 명령 실행 래퍼.
//
// V1 의 모든 destructive 작업은 IOCTL 직접 호출 대신 PowerShell Storage 모듈 cmdlet
// (Initialize-Disk / New-Partition / Format-Volume / Set-Volume / Remove-Partition 등) 을
// 통해 수행된다. 이 모듈은 그 cmdlet 들을 안전하게 spawn 하고 stdout/stderr 를 캡처해
// `ParqError::Platform` 으로 변환한다.
//
// 보안 / 안전:
// - `-NoProfile -NonInteractive` 로 사용자 프로파일 영향 차단
// - `-ExecutionPolicy Bypass` 는 외부 .ps1 을 실행하지 않으므로 안전 (인라인 스크립트만)
// - shell metacharacters 가 포함될 수 있는 사용자 입력은 호출자가 ' ' 로 quote, ' 는 ''
//   (PowerShell single-quote 이스케이프) 으로 처리해야 한다 — `quote_single` 헬퍼 제공.

use std::process::Command;

use serde::de::DeserializeOwned;
use tracing::{debug, instrument, warn};

use crate::{ParqError, Result};

/// PowerShell 실행 결과의 stdout / stderr.
#[derive(Debug)]
pub struct PsOutput {
    pub stdout: String,
    pub stderr: String,
}

/// 인라인 PowerShell 스크립트를 실행하고 stdout/stderr 를 반환한다.
///
/// 비-zero 종료 코드는 `ParqError::Platform` 으로 변환되며 stderr 가 메시지에 포함된다.
/// 호출자가 입력을 quote 할 책임이 있다 — 사용자 입력을 직접 끼워 넣을 때는 `quote_single`
/// 사용 권장.
#[instrument(skip(script), fields(script_len = script.len()))]
pub fn run_command(script: &str) -> Result<PsOutput> {
    debug!(target: "parq::powershell", script = %script, "PowerShell 실행");
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .output()
        .map_err(|e| ParqError::Platform(format!("powershell.exe 실행 실패: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        warn!(
            target: "parq::powershell",
            exit = code,
            stderr = %stderr.trim(),
            "PowerShell 실패"
        );
        return Err(ParqError::Platform(format!(
            "PowerShell 실패 (exit={code}): {}",
            stderr.trim()
        )));
    }
    Ok(PsOutput { stdout, stderr })
}

/// 스크립트가 `ConvertTo-Json` 으로 출력한 JSON 을 역직렬화해서 반환한다.
///
/// 단일 객체와 단일-요소 배열의 비대칭을 흡수하기 위해 `-Depth 5` 와 함께 호출자가
/// `@(...)` 로 명시적 배열 wrapping 을 하길 권장.
#[instrument(skip(script), fields(script_len = script.len()))]
pub fn run_json<T: DeserializeOwned>(script: &str) -> Result<T> {
    let out = run_command(script)?;
    let trimmed = out.stdout.trim();
    serde_json::from_str(trimmed).map_err(|e| {
        ParqError::Platform(format!(
            "PowerShell JSON 파싱 실패: {e}\n원본 stdout: {}",
            if trimmed.len() > 500 {
                format!("{}…(truncated)", &trimmed[..500])
            } else {
                trimmed.to_string()
            }
        ))
    })
}

/// PowerShell single-quoted 문자열로 안전하게 quote 한다.
/// PowerShell 에서 single-quoted string 안의 ' 는 '' 로 이스케이프한다.
#[must_use]
pub fn quote_single(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_single_basic() {
        assert_eq!(quote_single("hello"), "'hello'");
    }

    #[test]
    fn quote_single_escapes_apostrophe() {
        assert_eq!(quote_single("it's"), "'it''s'");
    }

    #[test]
    fn quote_single_handles_empty() {
        assert_eq!(quote_single(""), "''");
    }

    #[test]
    fn quote_single_preserves_metacharacters() {
        // single-quoted 안에서는 $ ` 등이 literal 로 처리됨.
        assert_eq!(quote_single("$var; rm -rf /"), "'$var; rm -rf /'");
    }
}
