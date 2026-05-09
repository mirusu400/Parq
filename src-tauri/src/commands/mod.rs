// Tauri command 핸들러 모듈.
//
// 컨벤션:
// - read-only command 와 write command 는 파일을 분리한다 (read.rs / write.rs).
// - write command 는 함수명에 _dangerous 접미사를 붙인다.
// - 모든 command 는 입력 검증 후 safety::guard 통과해야 실행한다.

pub mod read;
pub mod write;
