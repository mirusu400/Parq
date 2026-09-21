<#
.SYNOPSIS
  실제 파일 기반 VHD에서 Parq MBR/GPT 이동 엔진의 kill/resume 경로를 검증한다.

.DESCRIPTION
  Hyper-V PowerShell 모듈 없이 Windows 기본 diskpart로 새 64 MiB VHD를 만든다.
  생성 직후 파일 경로, 크기, 시스템/부팅 여부를 검증한 디스크에만 raw write를 허용한다.
  비겹침, 마지막 청크 경계, 좌/우 겹침, flush 후 checkpoint 전, partition table write 전/후에
  강제 종료하고 재개한다. 독립 SHA256과 Windows 파티션 재열거 결과를 확인하며,
  VHD는 종료 시 항상 분리한다.

.PARAMETER OutDir
  VHD와 체크포인트를 저장할 디렉터리. 기본값은 저장소의 test-vhds 디렉터리다.
#>

[CmdletBinding()]
param(
    [string]$OutDir,
    [ValidateSet("MBR", "GPT")]
    [string]$PartitionStyle = "MBR"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ([string]::IsNullOrWhiteSpace($OutDir)) {
    $OutDir = Join-Path $PSScriptRoot "..\test-vhds"
}

$VhdSizeBytes = 64MB
$MaxGuardBytes = 128MB
$TestSectorSize = 512
$PartitionSizeBytes = 8MB
$PartitionLengthSectors = [long]($PartitionSizeBytes / $TestSectorSize)

function Assert-Admin {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "관리자 권한 PowerShell에서 실행해야 합니다."
    }
}

function Invoke-DiskPart {
    param([Parameter(Mandatory)][string[]]$Commands)

    @($Commands) + "exit" | diskpart.exe
    if ($LASTEXITCODE -ne 0) {
        throw "diskpart 실패: exit=$LASTEXITCODE"
    }
}

function Get-ValidatedTestDisk {
    param([Parameter(Mandatory)][string]$VhdPath)

    $image = Get-DiskImage -ImagePath $VhdPath
    if (-not $image.Attached) {
        throw "테스트 VHD가 attach 상태가 아닙니다: $VhdPath"
    }
    $disks = @($image | Get-Disk)
    if ($disks.Count -ne 1) {
        throw "테스트 VHD에 연결된 디스크가 정확히 하나가 아닙니다."
    }
    $disk = $disks[0]
    if ($disk.IsSystem -or $disk.IsBoot -or $disk.IsReadOnly) {
        throw "가드 위반: 시스템/부팅/읽기전용 디스크에는 테스트할 수 없습니다."
    }
    if ([long]$disk.Size -ne $VhdSizeBytes -or [long]$disk.Size -gt $MaxGuardBytes) {
        throw "가드 위반: 테스트 디스크 크기가 예상과 다릅니다: $($disk.Size)"
    }
    if (-not [string]::Equals([string]$disk.Location, $VhdPath, [StringComparison]::OrdinalIgnoreCase)) {
        throw "가드 위반: 디스크 Location과 생성한 VHD 경로가 다릅니다."
    }
    if ([string]$disk.BusType -ne "File Backed Virtual") {
        throw "가드 위반: 파일 기반 가상 디스크가 아닙니다: $($disk.BusType)"
    }
    return $disk
}

function Remove-TestDriveLetter {
    param([Parameter(Mandatory)][int]$DiskNumber)

    $partition = Get-Partition -DiskNumber $DiskNumber | Select-Object -First 1
    $letter = [string]$partition.DriveLetter
    if ($letter -and $letter[0] -ne [char]0) {
        Remove-PartitionAccessPath -DiskNumber $DiskNumber `
            -PartitionNumber $partition.PartitionNumber -AccessPath "$letter`:\" -Confirm:$false
        Start-Sleep -Milliseconds 500
    }
}

function Assert-MovedPartition {
    param(
        [Parameter(Mandatory)][int]$DiskNumber,
        [Parameter(Mandatory)][long]$ExpectedStartLba,
        [Parameter(Mandatory)][string]$ExpectedSha256
    )

    Update-HostStorageCache
    $partitions = @(Get-Partition -DiskNumber $DiskNumber)
    if ($partitions.Count -ne 1) {
        throw "파티션 수가 예상과 다릅니다: $($partitions.Count)"
    }
    $expectedOffset = $ExpectedStartLba * $TestSectorSize
    if ([long]$partitions[0].Offset -ne $expectedOffset) {
        throw "파티션 시작 오프셋 불일치: actual=$($partitions[0].Offset), expected=$expectedOffset"
    }
    if ([long]$partitions[0].Size -ne $PartitionSizeBytes) {
        throw "파티션 크기 불일치: $($partitions[0].Size)"
    }
    $hash = Get-RawRegionHash -DiskNumber $DiskNumber -StartLba $ExpectedStartLba `
        -LengthSectors $PartitionLengthSectors -SectorSize $TestSectorSize
    if ($hash.Sha256 -ne $ExpectedSha256) {
        throw "독립 SHA256 불일치: actual=$($hash.Sha256), expected=$ExpectedSha256"
    }
}

function Invoke-KillResumeMove {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][int]$DiskNumber,
        [Parameter(Mandatory)][long]$SourceLba,
        [Parameter(Mandatory)][long]$DestinationLba,
        [Parameter(Mandatory)][long]$KillAt,
        [ValidateSet("before_cursor", "after_cursor")]
        [string]$KillPhase = "after_cursor",
        [Parameter(Mandatory)][string]$CheckpointPath,
        [Parameter(Mandatory)][string]$ExpectedSha256,
        [Parameter(Mandatory)][string]$MoveRegionExe,
        [Parameter(Mandatory)][string]$MovePartExe
    )

    Remove-TestDriveLetter -DiskNumber $DiskNumber
    & $MoveRegionExe $DiskNumber $MaxGuardBytes $SourceLba $DestinationLba `
        $PartitionLengthSectors $CheckpointPath $KillAt $KillPhase
    $killExit = $LASTEXITCODE
    if ($killExit -eq 0) {
        throw "$Name`: kill 지점에서 프로세스가 종료되지 않았습니다."
    }
    & $MovePartExe $DiskNumber $MaxGuardBytes $SourceLba $DestinationLba $CheckpointPath
    if ($LASTEXITCODE -ne 0) {
        throw "$Name`: move_partition 재개 실패: exit=$LASTEXITCODE"
    }
    Assert-MovedPartition -DiskNumber $DiskNumber -ExpectedStartLba $DestinationLba `
        -ExpectedSha256 $ExpectedSha256
    Write-Host "[PASS] $Name" -ForegroundColor Green
}

function Invoke-TableKillResumeMove {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][int]$DiskNumber,
        [Parameter(Mandatory)][long]$SourceLba,
        [Parameter(Mandatory)][long]$DestinationLba,
        [ValidateSet("before_table_write", "after_gpt_backup", "after_gpt_primary_entries", "after_table_write")]
        [Parameter(Mandatory)][string]$KillPhase,
        [Parameter(Mandatory)][string]$CheckpointPath,
        [Parameter(Mandatory)][string]$ExpectedSha256,
        [Parameter(Mandatory)][string]$MovePartExe
    )

    Remove-TestDriveLetter -DiskNumber $DiskNumber
    & $MovePartExe $DiskNumber $MaxGuardBytes $SourceLba $DestinationLba $CheckpointPath $KillPhase
    $killExit = $LASTEXITCODE
    if ($killExit -eq 0) {
        throw "$Name`: table kill 지점에서 프로세스가 종료되지 않았습니다."
    }
    & $MovePartExe $DiskNumber $MaxGuardBytes $SourceLba $DestinationLba $CheckpointPath
    if ($LASTEXITCODE -ne 0) {
        throw "$Name`: table 복구 실패: exit=$LASTEXITCODE"
    }
    Assert-MovedPartition -DiskNumber $DiskNumber -ExpectedStartLba $DestinationLba `
        -ExpectedSha256 $ExpectedSha256
    Write-Host "[PASS] $Name" -ForegroundColor Green
}

function Assert-ReservedRegionRejected {
    param(
        [Parameter(Mandatory)][int]$DiskNumber,
        [Parameter(Mandatory)][long]$SourceLba,
        [Parameter(Mandatory)][string]$CheckpointPath,
        [Parameter(Mandatory)][string]$ExpectedSha256,
        [Parameter(Mandatory)][string]$MovePartExe
    )

    $reservedBefore = Get-RawRegionHash -DiskNumber $DiskNumber -StartLba 0 `
        -LengthSectors 2048 -SectorSize $TestSectorSize
    & $MovePartExe $DiskNumber $MaxGuardBytes $SourceLba 0 $CheckpointPath
    if ($LASTEXITCODE -eq 0) {
        throw "reserved-region guard: LBA 0 이동 요청이 거부되지 않았습니다."
    }
    $reservedAfter = Get-RawRegionHash -DiskNumber $DiskNumber -StartLba 0 `
        -LengthSectors 2048 -SectorSize $TestSectorSize
    if ($reservedAfter.Sha256 -ne $reservedBefore.Sha256) {
        throw "reserved-region guard: 거부된 요청이 예약 영역을 변경했습니다."
    }
    Assert-MovedPartition -DiskNumber $DiskNumber -ExpectedStartLba $SourceLba `
        -ExpectedSha256 $ExpectedSha256
    Write-Host "[PASS] reserved-region LBA 0 rejected without writes" -ForegroundColor Green
}

Assert-Admin

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$outPath = (New-Item -ItemType Directory -Path $OutDir -Force).FullName
$runId = "{0}-{1}" -f (Get-Date -Format "yyyyMMdd-HHmmss"), $PID
$vhdPath = Join-Path $outPath "actual-kill-$runId.vhd"
$checkpointDir = Join-Path $outPath "actual-kill-$runId"
New-Item -ItemType Directory -Path $checkpointDir | Out-Null

$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if ($null -eq $cargo) {
    $fallback = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
    if (-not (Test-Path -LiteralPath $fallback)) {
        throw "cargo를 찾을 수 없습니다."
    }
    $cargoPath = $fallback
}
else {
    $cargoPath = $cargo.Source
}

$manifestPath = Join-Path $repoRoot "src-tauri\Cargo.toml"
& $cargoPath build --manifest-path $manifestPath --example raw_write --example move_region --example move_part
if ($LASTEXITCODE -ne 0) {
    throw "kill-test 예제 빌드 실패: exit=$LASTEXITCODE"
}

$exampleDir = Join-Path $repoRoot "src-tauri\target\debug\examples"
$rawWriteExe = Join-Path $exampleDir "raw_write.exe"
$moveRegionExe = Join-Path $exampleDir "move_region.exe"
$movePartExe = Join-Path $exampleDir "move_part.exe"

. (Join-Path $PSScriptRoot "hash-region.ps1")

$attached = $false
try {
    if (Test-Path -LiteralPath $vhdPath) {
        throw "안전 중단: 새 VHD 경로가 이미 존재합니다: $vhdPath"
    }
    Invoke-DiskPart -Commands @(
        "create vdisk file=`"$vhdPath`" maximum=64 type=expandable",
        "select vdisk file=`"$vhdPath`"",
        "attach vdisk"
    )
    $attached = $true
    Start-Sleep -Seconds 1

    $disk = Get-ValidatedTestDisk -VhdPath $vhdPath
    $diskNumber = [int]$disk.Number
    Invoke-DiskPart -Commands @(
        "select disk $diskNumber",
        "clean",
        "convert $($PartitionStyle.ToLowerInvariant())",
        "create partition primary offset=1024 size=8"
    )
    Start-Sleep -Seconds 1

    $disk = Get-ValidatedTestDisk -VhdPath $vhdPath
    $partitions = @(Get-Partition -DiskNumber $diskNumber)
    if ($partitions.Count -ne 1 -or [long]$partitions[0].Offset -ne 1MB -or `
        [long]$partitions[0].Size -ne $PartitionSizeBytes) {
        throw "초기 $PartitionStyle 테스트 레이아웃 검증 실패"
    }

    $env:PARQ_ENABLE_V2_DESTRUCTIVE = "1"
    $env:PARQ_DEV_ALLOW_INTERNAL_DISKS = "1"

    $initialLba = 2048L
    & $rawWriteExe $diskNumber $MaxGuardBytes $initialLba $PartitionLengthSectors
    if ($LASTEXITCODE -ne 0) {
        throw "초기 패턴 write 실패: exit=$LASTEXITCODE"
    }
    $seedHash = Get-RawRegionHash -DiskNumber $diskNumber -StartLba $initialLba `
        -LengthSectors $PartitionLengthSectors -SectorSize $TestSectorSize

    Invoke-KillResumeMove -Name "non-overlap kill@3" -DiskNumber $diskNumber `
        -SourceLba 2048 -DestinationLba 32768 -KillAt 3 `
        -CheckpointPath (Join-Path $checkpointDir "non-overlap.json") `
        -ExpectedSha256 $seedHash.Sha256 -MoveRegionExe $moveRegionExe -MovePartExe $movePartExe
    Invoke-KillResumeMove -Name "last-chunk boundary kill@8" -DiskNumber $diskNumber `
        -SourceLba 32768 -DestinationLba 65536 -KillAt 8 `
        -CheckpointPath (Join-Path $checkpointDir "last-chunk.json") `
        -ExpectedSha256 $seedHash.Sha256 -MoveRegionExe $moveRegionExe -MovePartExe $movePartExe
    Invoke-KillResumeMove -Name "overlap-left forward kill@4" -DiskNumber $diskNumber `
        -SourceLba 65536 -DestinationLba 57344 -KillAt 4 `
        -CheckpointPath (Join-Path $checkpointDir "overlap-left.json") `
        -ExpectedSha256 $seedHash.Sha256 -MoveRegionExe $moveRegionExe -MovePartExe $movePartExe
    Invoke-KillResumeMove -Name "overlap-right backward kill@4" -DiskNumber $diskNumber `
        -SourceLba 57344 -DestinationLba 65536 -KillAt 4 `
        -CheckpointPath (Join-Path $checkpointDir "overlap-right.json") `
        -ExpectedSha256 $seedHash.Sha256 -MoveRegionExe $moveRegionExe -MovePartExe $movePartExe
    Invoke-KillResumeMove -Name "small-overlap-right before-cursor kill@4" -DiskNumber $diskNumber `
        -SourceLba 65536 -DestinationLba 66560 -KillAt 4 -KillPhase "before_cursor" `
        -CheckpointPath (Join-Path $checkpointDir "before-cursor-right.json") `
        -ExpectedSha256 $seedHash.Sha256 -MoveRegionExe $moveRegionExe -MovePartExe $movePartExe
    Invoke-KillResumeMove -Name "small-overlap-left before-cursor kill@4" -DiskNumber $diskNumber `
        -SourceLba 66560 -DestinationLba 65536 -KillAt 4 -KillPhase "before_cursor" `
        -CheckpointPath (Join-Path $checkpointDir "before-cursor-left.json") `
        -ExpectedSha256 $seedHash.Sha256 -MoveRegionExe $moveRegionExe -MovePartExe $movePartExe
    $scenarioCount = 8
    if ($PartitionStyle -eq "GPT") {
        Invoke-TableKillResumeMove -Name "after backup GPT write" -DiskNumber $diskNumber `
            -SourceLba 65536 -DestinationLba 32768 -KillPhase "after_gpt_backup" `
            -CheckpointPath (Join-Path $checkpointDir "after-gpt-backup.json") `
            -ExpectedSha256 $seedHash.Sha256 -MovePartExe $movePartExe
        Invoke-TableKillResumeMove -Name "after primary GPT entries write" -DiskNumber $diskNumber `
            -SourceLba 32768 -DestinationLba 65536 -KillPhase "after_gpt_primary_entries" `
            -CheckpointPath (Join-Path $checkpointDir "after-gpt-primary-entries.json") `
            -ExpectedSha256 $seedHash.Sha256 -MovePartExe $movePartExe
        $scenarioCount = 10
    }
    Invoke-TableKillResumeMove -Name "before $PartitionStyle table write" -DiskNumber $diskNumber `
        -SourceLba 65536 -DestinationLba 32768 -KillPhase "before_table_write" `
        -CheckpointPath (Join-Path $checkpointDir "before-table-write.json") `
        -ExpectedSha256 $seedHash.Sha256 -MovePartExe $movePartExe
    Invoke-TableKillResumeMove -Name "after $PartitionStyle table write" -DiskNumber $diskNumber `
        -SourceLba 32768 -DestinationLba 65536 -KillPhase "after_table_write" `
        -CheckpointPath (Join-Path $checkpointDir "after-table-write.json") `
        -ExpectedSha256 $seedHash.Sha256 -MovePartExe $movePartExe

    Assert-ReservedRegionRejected -DiskNumber $diskNumber -SourceLba 65536 `
        -CheckpointPath (Join-Path $checkpointDir "reserved-region-rejected.json") `
        -ExpectedSha256 $seedHash.Sha256 -MovePartExe $movePartExe

    Write-Host "`nactual $PartitionStyle VHD test: $scenarioCount kill/resume + 1 reserved-region guard passed, 0 failed" -ForegroundColor Green
    Write-Host "VHD: $vhdPath"
    Write-Host "SHA256: $($seedHash.Sha256)"
}
finally {
    if ($attached) {
        try {
            Invoke-DiskPart -Commands @(
                "select vdisk file=`"$vhdPath`"",
                "detach vdisk"
            )
        }
        catch {
            Write-Warning "테스트 VHD 분리 실패: $($_.Exception.Message)"
        }
    }
}
