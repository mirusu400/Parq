<#
.SYNOPSIS
  Parq 통합 테스트용 VHDX 생성.

.DESCRIPTION
  지정된 경로에 동적 확장 VHDX를 만들고, GPT로 초기화한 뒤
  여러 파티션 시나리오 (FAT32 / exFAT / NTFS / 미할당) 를 구성한다.
  실제 물리 디스크는 절대 건드리지 않는다.

  관리자 권한 PowerShell 에서 실행해야 한다.

.PARAMETER Path
  VHDX 파일 경로. 기본값: $env:USERPROFILE\parq-test.vhdx

.PARAMETER SizeGB
  VHDX 크기 (GB). 기본값: 10

.EXAMPLE
  .\scripts\create-test-vhd.ps1 -Path "$env:USERPROFILE\parq-test.vhdx" -SizeGB 10
#>

[CmdletBinding()]
param(
    [string]$Path = "$env:USERPROFILE\parq-test.vhdx",
    [int]$SizeGB = 10
)

$ErrorActionPreference = "Stop"

# 관리자 권한 체크
$current = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($current)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error "이 스크립트는 관리자 권한 PowerShell 에서 실행해야 합니다."
    exit 1
}

# 안전 가드: 물리 드라이브 경로 거부
if ($Path -match '^\\\\\.\\PhysicalDrive\d+$' -or $Path -match '^[a-zA-Z]:\\?$') {
    Write-Error "물리 드라이브 경로는 허용되지 않습니다. 파일 경로를 지정하세요."
    exit 1
}

if (Test-Path $Path) {
    Write-Warning "파일이 이미 존재합니다: $Path"
    $confirm = Read-Host "덮어쓰시겠습니까? (yes 입력)"
    if ($confirm -ne "yes") {
        Write-Host "취소됨."
        exit 0
    }
    Remove-Item -Path $Path -Force
}

Write-Host "VHDX 생성 중: $Path ($SizeGB GB)" -ForegroundColor Cyan

$sizeBytes = $SizeGB * 1GB
$vhd = New-VHD -Path $Path -SizeBytes $sizeBytes -Dynamic

Write-Host "VHDX 마운트 중..." -ForegroundColor Cyan
$mounted = Mount-VHD -Path $Path -PassThru
$disk = $mounted | Get-Disk

Write-Host "디스크 번호: $($disk.Number) / 친구 이름: $($disk.FriendlyName)" -ForegroundColor Green
Write-Host "  ※ 이 디스크 번호는 매번 바뀌니 하드코딩하지 마세요."

# GPT 초기화
Initialize-Disk -Number $disk.Number -PartitionStyle GPT -Confirm:$false

# 시나리오: FAT32 1GB, exFAT 2GB, NTFS 3GB, 나머지 미할당
Write-Host "파티션 생성 중..." -ForegroundColor Cyan

$p1 = New-Partition -DiskNumber $disk.Number -Size 1GB -AssignDriveLetter
Format-Volume -DriveLetter $p1.DriveLetter -FileSystem FAT32 -NewFileSystemLabel "PARQ-FAT32" -Confirm:$false | Out-Null
Write-Host "  [1] FAT32 1GB → $($p1.DriveLetter):" -ForegroundColor Green

$p2 = New-Partition -DiskNumber $disk.Number -Size 2GB -AssignDriveLetter
Format-Volume -DriveLetter $p2.DriveLetter -FileSystem exFAT -NewFileSystemLabel "PARQ-EXFAT" -Confirm:$false | Out-Null
Write-Host "  [2] exFAT 2GB → $($p2.DriveLetter):" -ForegroundColor Green

$p3 = New-Partition -DiskNumber $disk.Number -Size 3GB -AssignDriveLetter
Format-Volume -DriveLetter $p3.DriveLetter -FileSystem NTFS -NewFileSystemLabel "PARQ-NTFS" -Confirm:$false | Out-Null
Write-Host "  [3] NTFS 3GB → $($p3.DriveLetter):" -ForegroundColor Green

Write-Host ""
Write-Host "완료." -ForegroundColor Green
Write-Host "디스크 번호: $($disk.Number)"
Write-Host "VHDX 경로: $Path"
Write-Host ""
Write-Host "테스트 후 마운트 해제: Dismount-VHD -Path '$Path'"
