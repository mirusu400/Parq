<#
.SYNOPSIS
  Parq V2 kill-test / 이동 테스트용 VHD 커버링 세트 + manifest 생성.

.DESCRIPTION
  docs/v2-test-infrastructure.md §1 의 VHD 매트릭스를 구현한다. 이동 알고리즘 분기
  (섹터크기 × 파티션스타일 × 방향 × 겹침) 를 커버하는 픽스처 디스크들을 만든다.

  각 케이스마다:
    1. 동적 VHDX 생성 (New-VHD) — **파일 백업 가상 디스크. 물리 디스크 절대 안 건드림.**
    2. Mount 후 "방금 우리가 붙인 이 VHD 가 맞는지" 재확인 (raw 쓰기 전 필수 가드).
    3. MBR/GPT 초기화 + 파티션 레이아웃 (데이터 파티션 사이·뒤에 미할당 gap → 이동 여지).
    4. 각 파티션 extent 에 **결정론적 LBA-스탬프 패턴** raw write (byte-exact 검증용).
    5. hash-region.ps1 의 Get-RawRegionHash 로 각 파티션 raw SHA256 계산 → manifest.
    6. Dismount.

  안전 설계:
    - 물리 드라이브 경로는 입력·대상 어디서도 허용 안 함.
    - raw write 는 오직 이번 실행에서 우리가 New-VHD → Mount 한 디스크에만. Get-Disk.Location 이
      우리 VHD 파일 경로와 일치하고, IsSystem/IsBoot 가 아니어야 진행 (Assert-IsOurFreshVhd).
    - 커버링 세트(각 축 값 최소 1회) — 전체 카테시안 아님. 위험 조합(4Kn×GPT)은 강제 포함.

  근거: docs/v2-test-infrastructure.md §1, docs/v2-move-algorithm.md §2·§4, v2-checkpoint-format.md §2·§5.

  관리자 권한 PowerShell 필요.

.PARAMETER OutDir
  VHDX 와 manifest 를 쓸 디렉토리. 기본 .\test-vhds\

.PARAMETER Cases
  생성할 케이스 이름들 (미지정 시 전체 커버링 세트). 예: -Cases 512-gpt,4096-gpt

.PARAMETER SizeMB
  각 VHD 크기(MB). 기본 256 (빠른 반복). 현실적 테스트는 1024+ 권장.

.EXAMPLE
  .\scripts\vhd-matrix.ps1 -OutDir .\test-vhds\
#>

[CmdletBinding()]
param(
    [string]$OutDir = ".\test-vhds",
    [string[]]$Cases,
    [ValidateRange(64, 65536)]
    [int]$SizeMB = 256
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# 해시 oracle 함수(Get-RawRegionHash) dot-source
. (Join-Path $PSScriptRoot "hash-region.ps1")

function Assert-Admin {
    $current = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($current)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "이 스크립트는 관리자 권한 PowerShell 에서 실행해야 합니다."
    }
}

# 커버링 세트 정의. sector = 논리 섹터 크기, style = 파티션 테이블.
# 위험 조합 4Kn×GPT 포함(docs/v2-test-infrastructure.md §1).
$AllCases = @(
    @{ Name = "512-gpt";  Sector = 512;  Style = "GPT" }
    @{ Name = "4096-gpt"; Sector = 4096; Style = "GPT" }
    @{ Name = "512-mbr";  Sector = 512;  Style = "MBR" }
    @{ Name = "4096-mbr"; Sector = 4096; Style = "MBR" }
)

<#
  raw 쓰기 직전 안전 가드. 이 디스크가 정말 우리가 방금 만든 VHD 인지 확인한다.
  하나라도 어긋나면 throw — 실수로 실제 디스크에 패턴을 쓰는 것을 원천 차단.
#>
function Assert-IsOurFreshVhd {
    param(
        [Parameter(Mandatory)][int]$DiskNumber,
        [Parameter(Mandatory)][string]$VhdPath
    )
    $disk = Get-Disk -Number $DiskNumber
    if ($null -eq $disk) { throw "디스크 $DiskNumber 없음." }
    if ($disk.IsSystem -or $disk.IsBoot) {
        throw "가드 위반: 디스크 $DiskNumber 는 시스템/부팅 디스크. raw write 거부."
    }
    if ($disk.BusType -ne "File Backed Virtual" -and $disk.FriendlyName -notmatch "Virtual") {
        throw "가드 위반: 디스크 $DiskNumber 는 가상(VHD) 디스크가 아님 (BusType=$($disk.BusType)). raw write 거부."
    }
    $expected = (Resolve-Path $VhdPath).Path
    $actual = $null
    try { $actual = (Get-Disk -Number $DiskNumber).Location } catch {}
    if ([string]::IsNullOrWhiteSpace($actual) -or ($actual -ne $expected)) {
        throw "가드 위반: 디스크 $DiskNumber 의 Location('$actual') 이 우리 VHD('$expected') 와 불일치. raw write 거부."
    }
}

<#
  파티션 extent 에 결정론적 패턴을 raw write 한다. 섹터마다 앞 8바이트에 절대 LBA(LE),
  나머지는 (LBA XOR seed) 바이트로 채움 → 오프셋/정렬 오류가 있으면 해시가 달라져 검출됨.
  Assert-IsOurFreshVhd 통과 후에만 호출.
#>
function Write-DeterministicPattern {
    param(
        [Parameter(Mandatory)][int]$DiskNumber,
        [Parameter(Mandatory)][long]$StartLba,
        [Parameter(Mandatory)][long]$LengthSectors,
        [Parameter(Mandatory)][int]$SectorSize,
        [Parameter(Mandatory)][string]$VhdPath,
        [byte]$Seed = 0xA5
    )
    Assert-IsOurFreshVhd -DiskNumber $DiskNumber -VhdPath $VhdPath

    $path = "\\.\PhysicalDrive$DiskNumber"
    $chunkSectors = [long]([math]::Max(1, [math]::Floor(1MB / $SectorSize)))
    $fs = $null
    try {
        # Write 접근: 방금 만든 미포맷 VHD 파티션이라 볼륨 락 없음.
        $fs = New-Object System.IO.FileStream(
            $path,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::ReadWrite,
            [int]($chunkSectors * $SectorSize),
            [System.IO.FileOptions]::WriteThrough
        )
        $fs.Position = $StartLba * $SectorSize

        [long]$written = 0
        while ($written -lt $LengthSectors) {
            $n = [long][math]::Min($chunkSectors, ($LengthSectors - $written))
            $buf = New-Object byte[] ([int]($n * $SectorSize))
            for ($s = 0; $s -lt $n; $s++) {
                $absLba = $StartLba + $written + $s
                $base = [int]($s * $SectorSize)
                $lbaBytes = [System.BitConverter]::GetBytes([long]$absLba)  # 8B LE
                [System.Array]::Copy($lbaBytes, 0, $buf, $base, 8)
                $fill = [byte](($absLba -band 0xFF) -bxor $Seed)
                for ($b = 8; $b -lt $SectorSize; $b++) { $buf[$base + $b] = $fill }
            }
            $fs.Write($buf, 0, $buf.Length)
            $written += $n
        }
        $fs.Flush($true)
    }
    finally {
        if ($null -ne $fs) { $fs.Dispose() }
    }
}

Assert-Admin

$selected = if ($Cases) { $AllCases | Where-Object { $Cases -contains $_.Name } } else { $AllCases }
if (-not $selected) { throw "선택된 케이스 없음. 사용 가능: $($AllCases.Name -join ', ')" }

$OutDir = (New-Item -ItemType Directory -Force -Path $OutDir).FullName
Write-Host "출력 디렉토리: $OutDir" -ForegroundColor Cyan

foreach ($case in $selected) {
    $name = $case.Name
    $sector = [int]$case.Sector
    $style = [string]$case.Style
    $vhdPath = Join-Path $OutDir "case-$name.vhdx"
    $manifestPath = Join-Path $OutDir "case-$name.manifest.json"

    Write-Host "`n=== 케이스 $name (sector=$sector, style=$style, ${SizeMB}MB) ===" -ForegroundColor Yellow

    if ($vhdPath -match '^\\\\\.\\PhysicalDrive\d+$') { throw "물리 경로 거부: $vhdPath" }
    if (Test-Path $vhdPath) { Remove-Item $vhdPath -Force }

    $mounted = $null
    try {
        # 4Kn VHD 는 환경에 따라 미지원일 수 있음(docs §7) → 실패 시 스킵.
        try {
            New-VHD -Path $vhdPath -SizeBytes ($SizeMB * 1MB) -Dynamic `
                -LogicalSectorSizeBytes $sector -PhysicalSectorSizeBytes $sector | Out-Null
        }
        catch {
            Write-Warning "케이스 $name 스킵: New-VHD 가 sector=$sector 를 거부함 ($($_.Exception.Message))"
            if (Test-Path $vhdPath) { Remove-Item $vhdPath -Force }
            continue
        }

        $mounted = Mount-VHD -Path $vhdPath -PassThru
        $disk = $mounted | Get-Disk
        $diskNumber = [int]$disk.Number
        Write-Host "  마운트: 디스크 $diskNumber (하드코딩 금지 — 실행마다 다름)"

        # raw write 전 가드
        Assert-IsOurFreshVhd -DiskNumber $diskNumber -VhdPath $vhdPath

        Initialize-Disk -Number $diskNumber -PartitionStyle $style -Confirm:$false | Out-Null

        # 레이아웃: [P1 64MiB][gap 32MiB][P2 48MiB][trailing free]
        # P2(48MiB) < 뒤 free → 오른쪽 overlap 이동 시나리오 가능(move §2 경우 B).
        $p1Size = 64MB
        $gap = 32MB
        $p2Size = 48MB

        $part1 = New-Partition -DiskNumber $diskNumber -Size $p1Size
        # gap 을 두기 위해 P2 는 offset 지정 생성
        $p2Offset = $part1.Offset + $p1Size + $gap
        $part2 = New-Partition -DiskNumber $diskNumber -Offset $p2Offset -Size $p2Size

        $partitions = @($part1, $part2) | Sort-Object Offset

        $manifestParts = @()
        foreach ($p in $partitions) {
            $startLba = [long]($p.Offset / $sector)
            $lenSectors = [long]($p.Size / $sector)

            Write-Host "  패턴 write: part@offset=$($p.Offset) startLba=$startLba sectors=$lenSectors"
            Write-DeterministicPattern -DiskNumber $diskNumber -StartLba $startLba `
                -LengthSectors $lenSectors -SectorSize $sector -VhdPath $vhdPath

            $h = Get-RawRegionHash -DiskNumber $diskNumber -StartLba $startLba `
                -LengthSectors $lenSectors -SectorSize $sector
            Write-Host "    sha256=$($h.Sha256)" -ForegroundColor Green

            $manifestParts += [PSCustomObject]@{
                partitionNumber   = [int]$p.PartitionNumber
                startingOffsetBytes = [long]$p.Offset
                sizeBytes         = [long]$p.Size
                startLba          = $startLba
                lengthSectors     = $lenSectors
                sha256            = $h.Sha256
            }
        }

        $manifest = [PSCustomObject]@{
            case             = $name
            vhdPath          = (Resolve-Path $vhdPath).Path
            logicalSectorSize = $sector
            partitionStyle   = $style
            diskSizeBytes    = [long]$disk.Size
            note             = "raw 패턴: 섹터당 앞8B=절대LBA(LE), 나머지=(LBA&0xFF)^0xA5. move 후 byte-exact 검증용."
            partitions       = $manifestParts
        }
        $manifest | ConvertTo-Json -Depth 6 | Out-File -FilePath $manifestPath -Encoding utf8
        Write-Host "  manifest → $manifestPath" -ForegroundColor Cyan
    }
    finally {
        if ($null -ne $mounted) {
            try { Dismount-VHD -Path $vhdPath } catch { Write-Warning "Dismount 실패: $($_.Exception.Message)" }
        }
    }
}

Write-Host "`n완료. 생성된 케이스: $(($selected.Name) -join ', ')" -ForegroundColor Green
Write-Host "각 case-*.vhdx 는 mount-test-vhd.ps1 로 다시 붙여 테스트에 사용." -ForegroundColor Gray
