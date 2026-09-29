# Parq WinPE 오프라인 시스템 파티션 이동

이 흐름은 현재 Windows가 사용하는 NTFS 파티션의 **시작 LBA를 옮겨야 할 때만** 사용한다.
끝 경계만 바꾸는 일반 C: 리사이즈는 Windows에서 Parq의 `리사이즈` 기능으로 처리한다.

현재 단계는 개발자/VM 검증용이다. GPT, BitLocker 완전 해제, 별도 체크포인트 파티션,
정확한 디스크 fingerprint가 모두 맞아야 실행된다. WinPE에서도 확인 문구를 다시 입력해야 한다.

## 준비

Windows ADK와 같은 버전의 WinPE add-on을 설치한다. ARM64 VM은 ARM64 WinPE와
ARM64 `offline_system_move.exe`가 모두 필요하다. ISO 빌더는 실행 파일의 PE machine 값을
확인해 아키텍처가 다르면 중단한다.

대상 디스크에는 이동 대상과 겹치지 않는 별도 체크포인트 볼륨이 있어야 한다. 현재 구현은 이
볼륨이 대상 GPT 디스크 안에 있을 것을 요구한다. 요청·상태·체크포인트 파일은 모두 이 볼륨에
저장된다.

## 1. 요청 생성

관리자 PowerShell에서 축소나 이동 전에 실행한다. 아래 숫자는 예시이며 실제 파티션 배치에
맞게 계산해야 한다.

```powershell
.\scripts\new-offline-system-move-request.ps1 `
  -SourceDriveLetter C `
  -NewStartBytes 272629760 `
  -ExpectedSourceSizeAfterBytes 53687091200 `
  -CheckpointDriveLetter R
```

스크립트는 디스크에 파티션 변경을 하지 않고 `R:\Parq\request.json`만 만든다. 새 영역이 다른
파티션이나 GPT 끝 예약 영역과 겹치면 요청 생성을 거부한다.

C: 축소가 필요하면 요청 생성 후 Parq의 온라인 리사이즈로
`ExpectedSourceSizeAfterBytes`와 정확히 같은 크기로 축소한다. 시작 LBA는 이 단계에서 바뀌지
않는다. 요청 생성 뒤 디스크 배치나 대상 크기가 달라졌다면 기존 요청을 사용하지 말고 다시 만든다.

## 2. ISO 생성

관리자 PowerShell에서 실행한다.

```powershell
.\scripts\build-winpe.ps1 -Architecture amd64
```

ARM64 바이너리를 별도로 빌드했다면 다음처럼 지정한다.

```powershell
.\scripts\build-winpe.ps1 -Architecture arm64 `
  -OfflineBinaryPath C:\build\offline_system_move.exe
```

결과 ISO는 기본적으로 `artifacts\winpe-<arch>-<timestamp>` 아래에 생성된다. 빌더는 ISO만
만들며 USB, BCD, VM 부팅 순서를 변경하지 않는다.

## 3. WinPE에서 점검과 실행

ISO로 부팅하면 런처가 각 볼륨에서 `\Parq\request.json`을 찾는다. 요청에 기록된 디스크 번호,
전체 크기, 파티션 시작 LBA를 먼저 대조하고 원본과 체크포인트 볼륨에 임시 드라이브 문자를
부여한다.

- 원본이 `expectedSourceSizeBefore`이면 `Preflight`만 가능하다. Windows로 돌아가 계획한
  크기로 축소한 뒤 다시 부팅한다.
- 원본이 `expectedSourceSizeAfter`이면 `Preflight` 또는 `Execute`를 선택할 수 있다.
- 두 크기와 모두 다르거나 fingerprint가 다르면 실행은 중단된다.

수동으로 다시 실행할 때는 다음 명령을 사용한다.

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File X:\Parq\Invoke-ParqOfflineMove.ps1 -Action Preflight

powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File X:\Parq\Invoke-ParqOfflineMove.ps1 -Action Execute
```

`Execute`는 원본 볼륨을 잠그고 분리한 뒤 checkpoint 기반 복사, SHA-256 검증, GPT backup/primary
갱신, NTFS hidden-sectors 갱신을 수행한다. 중간에 전원이 끊기면 같은 ISO와 같은 request로 다시
실행해 checkpoint 복구 경로로 진입한다.

## 재부팅 전 백업 규칙

개발 중 실제 WinPE 부팅이나 VM 재부팅으로 넘어가기 직전에는 작업 트리 변경을 커밋하고 원격
푸시 성공을 확인한다. 그 뒤 ISO SHA-256과 VM 스냅샷 상태를 기록하고 부팅 테스트를 시작한다.
