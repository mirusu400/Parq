use thiserror::Error;

#[derive(Error, Debug)]
pub enum ParqError {
    #[error("디스크를 찾을 수 없음: {0}")]
    DiskNotFound(String),

    #[error("시스템 파티션 보호: {0}")]
    SystemPartitionProtected(String),

    #[error("작업 검증 실패: {0}")]
    ValidationFailed(String),

    #[error("플랫폼 작업 실패: {0}")]
    Platform(String),

    #[error("트랜잭션 실패: {0}")]
    Transaction(String),

    #[error("아직 구현되지 않음: {0}")]
    NotImplemented(&'static str),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ParqError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_not_found_renders_korean_message() {
        let err = ParqError::DiskNotFound("Disk0".into());
        assert_eq!(err.to_string(), "디스크를 찾을 수 없음: Disk0");
    }

    #[test]
    fn system_partition_protected_renders() {
        let err = ParqError::SystemPartitionProtected("C:".into());
        assert_eq!(err.to_string(), "시스템 파티션 보호: C:");
    }

    #[test]
    fn validation_failed_renders() {
        let err = ParqError::ValidationFailed("free space 부족".into());
        assert_eq!(err.to_string(), "작업 검증 실패: free space 부족");
    }

    #[test]
    fn not_implemented_carries_static_str() {
        let err = ParqError::NotImplemented("partition::resize");
        assert_eq!(err.to_string(), "아직 구현되지 않음: partition::resize");
    }

    #[test]
    fn io_error_uses_transparent_message() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let err: ParqError = io.into();
        assert_eq!(err.to_string(), "missing");
    }
}
