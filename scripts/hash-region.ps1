<#
.SYNOPSIS
  디스크의 raw 섹터 범위 SHA256 해시 (무결성 oracle). **read-only.**

.DESCRIPTION
  Parq V2 kill-test / 이동 무결성 검증의 독립 기준(oracle) 이다.
  \\.\PhysicalDriveN 을 읽기 전용으로 열어 [StartLba, StartLba+LengthSectors) 구간의
  raw 바이트를 SHA256 한다. 이 스크립트는 **디스크에 절대 쓰지 않는다** — FileAccess::Read
  로만 연다.

  제품 코드(Rust) 가 같은 구간에 대해 동일 해시를 내야 한다 (docs/v2-raw-io.md §7, PR3 게이트).
  즉 이 PowerShell 값이 "정답", Rust 값이 "검증 대상".

  근거 문서:
    docs/v2-test-infrastructure.md §2  (해시 oracle 이중화)
    docs/v2-checkpoint-format.md   §5  (무결성 검증)
    docs/v2-raw-io.md              §3  (섹터 정렬)

  관리자 권한 PowerShell 필요 (raw 디스크 핸들).

.PARAMETER DiskNumber
  대상 디스크 번호 (Get-Disk 의 Number). ※ 재부팅마다 바뀌니 호출 직전 확인할 것.

.PARAMETER StartLba
  시작 논리 섹터 (0-based).

.PARAMETER LengthSectors
  해시할 섹터 수.

.PARAMETER SectorSize
  논리 섹터 크기(바이트). 미지정 시 Get-Disk 의 LogicalSectorSize 로 조회. 보통 512 또는 4096.

.PARAMETER AsObject
  해시 문자열 대신 PSCustomObject (DiskNumber/StartLba/LengthSectors/SectorSize/Sha256/Bytes) 반환.
  vhd-matrix.ps1 이 manifest 생성 시 이 함수를 dot-source 해서 사용한다.

.EXAMPLE
  .\scripts\hash-region.ps1 -DiskNumber 2 -StartLba 2048 -LengthSectors 409600

.EXAMPLE
  # 다른 스크립트에서 함수만 재사용
  . .\scripts\hash-region.ps1
  $h = Get-RawRegionHash -DiskNumber 2 -StartLba 2048 -LengthSectors 2048 -SectorSize 512
#>

[CmdletBinding()]
param(
    [int]$DiskNumber = -1,

    [long]$StartLba = -1,

    [long]$LengthSectors = 0,

    [ValidateSet(512, 4096)]
    [int]$SectorSize = 0,

    [switch]$AsObject
)

$ErrorActionPreference = "Stop"

function Assert-Admin {
    $current = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($current)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "이 스크립트는 관리자 권한 PowerShell 에서 실행해야 합니다 (raw 디스크 핸들)."
    }
}

<#
  raw 디스크 구간의 SHA256 을 계산한다. read-only.
  섹터 정렬(docs/v2-raw-io.md §3): 시작 위치·읽기 크기 모두 섹터 배수. StartLba/LengthSectors 가
  섹터 단위이므로 자동 정렬된다. 청크는 1 MiB 를 섹터 배수로 내림한 크기.
#>
function Get-RawRegionHash {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][int]$DiskNumber,
        [Parameter(Mandatory = $true)][long]$StartLba,
        [Parameter(Mandatory = $true)][long]$LengthSectors,
        [int]$SectorSize = 0
    )

    Assert-Admin

    # 섹터 크기 확정
    if ($SectorSize -le 0) {
        $disk = Get-Disk -Number $DiskNumber
        if ($null -eq $disk) { throw "디스크 번호 $DiskNumber 를 찾을 수 없습니다." }
        $SectorSize = [int]$disk.LogicalSectorSize
        if ($SectorSize -le 0) { $SectorSize = 512 }
    }

    $totalBytes = $LengthSectors * $SectorSize
    $startByte = $StartLba * $SectorSize

    # 청크: 1 MiB 를 섹터 배수로 내림 (최소 1 섹터)
    $chunkBytes = [math]::Floor(1MB / $SectorSize) * $SectorSize
    if ($chunkBytes -lt $SectorSize) { $chunkBytes = $SectorSize }

    $path = "\\.\PhysicalDrive$DiskNumber"

    # read-only + FileShare ReadWrite (열거 도구와 공존). 절대 Write 접근 요청하지 않는다.
    $fs = $null
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $fs = New-Object System.IO.FileStream(
            $path,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read,
            [System.IO.FileShare]::ReadWrite,
            [int]$chunkBytes,
            [System.IO.FileOptions]::None
        )
        $fs.Position = $startByte

        $buffer = New-Object byte[] ([int]$chunkBytes)
        [long]$remaining = $totalBytes

        while ($remaining -gt 0) {
            $want = [int][math]::Min($chunkBytes, $remaining)
            # raw 디바이스: 읽기 크기는 섹터 배수여야 한다. want 는 섹터 배수(위 계산)로 유지됨.
            [int]$got = 0
            [int]$offset = 0
            # 한 청크 내에서 부분 전송(raw-io §2.3) 대비 루프
            while ($offset -lt $want) {
                $n = $fs.Read($buffer, $offset, $want - $offset)
                if ($n -le 0) {
                    throw "예상보다 일찍 EOF/0바이트 read (position=$($fs.Position), want=$want). 구간이 디스크 범위를 넘었을 수 있습니다."
                }
                $offset += $n
            }
            $got = $offset
            [void]$sha.TransformBlock($buffer, 0, $got, $null, 0)
            $remaining -= $got
        }

        [void]$sha.TransformFinalBlock((New-Object byte[] 0), 0, 0)
        $hashHex = ([System.BitConverter]::ToString($sha.Hash)).Replace("-", "").ToLowerInvariant()

        return [PSCustomObject]@{
            DiskNumber    = $DiskNumber
            StartLba      = $StartLba
            LengthSectors = $LengthSectors
            SectorSize    = $SectorSize
            Bytes         = $totalBytes
            Sha256        = $hashHex
        }
    }
    finally {
        if ($null -ne $fs) { $fs.Dispose() }
        $sha.Dispose()
    }
}

# 스크립트로 직접 실행됐을 때만 CLI 로 동작 (dot-source 시엔 함수만 로드)
if ($MyInvocation.InvocationName -ne '.') {
    if ($DiskNumber -lt 0 -or $StartLba -lt 0 -or $LengthSectors -lt 1) {
        throw "DiskNumber, StartLba, LengthSectors 를 지정해야 합니다."
    }
    $result = Get-RawRegionHash -DiskNumber $DiskNumber -StartLba $StartLba `
        -LengthSectors $LengthSectors -SectorSize $SectorSize
    if ($AsObject) {
        $result
    }
    else {
        Write-Host "disk=$($result.DiskNumber) lba=$($result.StartLba) sectors=$($result.LengthSectors) sectorSize=$($result.SectorSize) bytes=$($result.Bytes)" -ForegroundColor Cyan
        Write-Host "SHA256 = $($result.Sha256)" -ForegroundColor Green
    }
}
