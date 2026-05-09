<#
.SYNOPSIS
  Parq 테스트용 VHDX 마운트 / 마운트 해제.

.DESCRIPTION
  -Action Mount : 지정된 VHDX 를 attach.
  -Action Dismount : detach.

  관리자 권한 PowerShell 에서 실행해야 한다.

.EXAMPLE
  .\scripts\mount-test-vhd.ps1 -Path "$env:USERPROFILE\parq-test.vhdx" -Action Mount
  .\scripts\mount-test-vhd.ps1 -Path "$env:USERPROFILE\parq-test.vhdx" -Action Dismount
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Path,

    [ValidateSet("Mount", "Dismount")]
    [string]$Action = "Mount"
)

$ErrorActionPreference = "Stop"

$current = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($current)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error "이 스크립트는 관리자 권한 PowerShell 에서 실행해야 합니다."
    exit 1
}

if (-not (Test-Path $Path)) {
    Write-Error "파일을 찾을 수 없습니다: $Path"
    exit 1
}

switch ($Action) {
    "Mount" {
        $mounted = Mount-VHD -Path $Path -PassThru
        $disk = $mounted | Get-Disk
        Write-Host "마운트 완료. 디스크 번호: $($disk.Number)" -ForegroundColor Green
    }
    "Dismount" {
        Dismount-VHD -Path $Path
        Write-Host "마운트 해제 완료: $Path" -ForegroundColor Green
    }
}
