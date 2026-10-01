<#
.SYNOPSIS
  현재 Windows 볼륨의 오프라인 이동 요청 JSON을 생성한다.

.DESCRIPTION
  이 스크립트는 디스크에 파티션 변경을 하지 않는다. 현재 디스크/볼륨 fingerprint와 이동 범위를
  검증한 뒤, WinPE에서 offline_system_move.exe가 사용할 request.json만 체크포인트 볼륨에 쓴다.

  C:를 축소해야 한다면 이 스크립트를 먼저 실행해 축소 전 크기를 기록하고, Parq의 온라인
  리사이즈로 정확히 ExpectedSourceSizeAfterBytes까지 축소한 다음 WinPE로 부팅한다.

.EXAMPLE
  .\scripts\new-offline-system-move-request.ps1 `
    -SourceDriveLetter C -NewStartBytes 272629760 `
    -ExpectedSourceSizeAfterBytes 53687091200 -CheckpointDriveLetter R
#>

[CmdletBinding()]
param(
    [ValidatePattern('^[A-Za-z]$')]
    [string]$SourceDriveLetter = "C",

    [Parameter(Mandatory)]
    [ValidateRange(1048576, [long]::MaxValue)]
    [long]$NewStartBytes,

    [Parameter(Mandatory)]
    [ValidateRange(1048576, [long]::MaxValue)]
    [long]$ExpectedSourceSizeAfterBytes,

    [Parameter(Mandatory)]
    [ValidatePattern('^[A-Za-z]$')]
    [string]$CheckpointDriveLetter,

    [ValidatePattern('^[A-WY-Za-wy-z]$')]
    [string]$WinPeCheckpointDriveLetter = "P",

    [string]$OutputPath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Get-SingleItem {
    param(
        [Parameter(Mandatory)][object[]]$Items,
        [Parameter(Mandatory)][string]$Description
    )

    if ($Items.Count -ne 1) {
        throw "Expected exactly one $Description item; found $($Items.Count)."
    }
    return $Items[0]
}

function Assert-BitLockerDisabled {
    param([Parameter(Mandatory)][string]$DriveLetter)

    $command = Get-Command Get-BitLockerVolume -ErrorAction SilentlyContinue
    if ($null -eq $command) {
        throw "Get-BitLockerVolume is unavailable, so BitLocker state cannot be verified."
    }
    $volume = Get-BitLockerVolume -MountPoint "$DriveLetter`:" -ErrorAction Stop
    if ([string]$volume.VolumeStatus -ne "FullyDecrypted") {
        throw "$DriveLetter`: must be fully decrypted. Current state: $($volume.VolumeStatus)"
    }
}

$sourceLetter = $SourceDriveLetter.ToUpperInvariant()
$checkpointLetter = $CheckpointDriveLetter.ToUpperInvariant()
$winPeCheckpointLetter = $WinPeCheckpointDriveLetter.ToUpperInvariant()
if ($sourceLetter -eq $checkpointLetter) {
    throw "Source and checkpoint volumes must be different."
}
if ($winPeCheckpointLetter -eq "X") {
    throw "X: is the WinPE RAM drive and cannot hold checkpoints."
}

$sourcePartition = Get-SingleItem `
    -Items @(Get-Partition -DriveLetter $sourceLetter -ErrorAction Stop) `
    -Description "$sourceLetter`: source partition"
$checkpointPartition = Get-SingleItem `
    -Items @(Get-Partition -DriveLetter $checkpointLetter -ErrorAction Stop) `
    -Description "$checkpointLetter`: checkpoint partition"

if ($sourcePartition.DiskNumber -eq $checkpointPartition.DiskNumber) {
    throw "The checkpoint volume must be on a different physical disk from the source."
}

$disk = Get-Disk -Number $sourcePartition.DiskNumber -ErrorAction Stop
if ([string]$disk.PartitionStyle -ne "GPT") {
    throw "Offline Windows partition moves require a GPT disk."
}
if ($disk.IsReadOnly) {
    throw "The target disk is read-only."
}
$checkpointDisk = Get-Disk -Number $checkpointPartition.DiskNumber -ErrorAction Stop
if ([string]$checkpointDisk.PartitionStyle -ne "GPT") {
    throw "The checkpoint disk must use GPT."
}
if ($checkpointDisk.IsReadOnly -or $checkpointDisk.IsSystem -or $checkpointDisk.IsBoot) {
    throw "The checkpoint disk must be writable and must not be a Windows system/boot disk."
}

$sourceVolume = Get-Volume -DriveLetter $sourceLetter -ErrorAction Stop
if ([string]$sourceVolume.FileSystem -ne "NTFS") {
    throw "The source volume must be NTFS. Current filesystem: $($sourceVolume.FileSystem)"
}
$checkpointVolume = Get-Volume -DriveLetter $checkpointLetter -ErrorAction Stop
if ([string]$checkpointVolume.FileSystem -notin @("NTFS", "FAT32", "exFAT")) {
    throw "The checkpoint volume must use NTFS, FAT32, or exFAT."
}
Assert-BitLockerDisabled -DriveLetter $sourceLetter
Assert-BitLockerDisabled -DriveLetter $checkpointLetter

$sectorSize = [long]$disk.LogicalSectorSize
if ($sectorSize -le 0) {
    throw "The logical sector size is unavailable."
}
if (($NewStartBytes % $sectorSize) -ne 0) {
    throw "NewStartBytes must align to the logical sector size ($sectorSize bytes)."
}
if (($ExpectedSourceSizeAfterBytes % $sectorSize) -ne 0) {
    throw "ExpectedSourceSizeAfterBytes must align to the logical sector size ($sectorSize bytes)."
}
if ($ExpectedSourceSizeAfterBytes -gt [long]$sourcePartition.Size) {
    throw "ExpectedSourceSizeAfterBytes exceeds the current source size."
}
if ($NewStartBytes -eq [long]$sourcePartition.Offset) {
    throw "The new start position equals the current start position."
}

$newEnd = [decimal]$NewStartBytes + [decimal]$ExpectedSourceSizeAfterBytes
if ($newEnd -gt ([long]$disk.Size - 1MB)) {
    throw "The destination reaches the reserved area at the end of the GPT disk."
}
foreach ($partition in @(Get-Partition -DiskNumber $disk.Number)) {
    if ($partition.PartitionNumber -eq $sourcePartition.PartitionNumber) {
        continue
    }
    $partitionStart = [long]$partition.Offset
    $partitionEnd = [decimal]$partitionStart + [decimal]$partition.Size
    if ($NewStartBytes -lt $partitionEnd -and $partitionStart -lt $newEnd) {
        throw "The destination overlaps partition #$($partition.PartitionNumber)."
    }
}

$rawDisk = Get-SingleItem `
    -Items @(Get-CimInstance -Namespace "ROOT\Microsoft\Windows\Storage" -ClassName MSFT_Disk |
        Where-Object { $_.Number -eq $disk.Number }) `
    -Description "MSFT_Disk #$($disk.Number)"
$model = ([string]$rawDisk.Model).Trim()
if ([string]::IsNullOrWhiteSpace($model)) {
    $model = "Unknown"
}
$serial = ([string]$rawDisk.SerialNumber).Trim()
if ([string]::IsNullOrWhiteSpace($serial)) {
    $serial = $null
}

$rawCheckpointDisk = Get-SingleItem `
    -Items @(Get-CimInstance -Namespace "ROOT\Microsoft\Windows\Storage" -ClassName MSFT_Disk |
        Where-Object { $_.Number -eq $checkpointDisk.Number }) `
    -Description "checkpoint MSFT_Disk #$($checkpointDisk.Number)"
$checkpointModel = ([string]$rawCheckpointDisk.Model).Trim()
if ([string]::IsNullOrWhiteSpace($checkpointModel)) {
    $checkpointModel = "Unknown"
}
$checkpointSerial = ([string]$rawCheckpointDisk.SerialNumber).Trim()
if ([string]::IsNullOrWhiteSpace($checkpointSerial)) {
    $checkpointSerial = $null
}

$checkpointSectorSize = [long]$checkpointDisk.LogicalSectorSize
if ($checkpointSectorSize -le 0) {
    throw "The checkpoint disk logical sector size is unavailable."
}
if (([long]$checkpointPartition.Offset % $checkpointSectorSize) -ne 0) {
    throw "The checkpoint partition offset is not sector-aligned."
}
$checkpointStartLba = [long]$checkpointPartition.Offset / $checkpointSectorSize
$sourceStartLba = [long]$sourcePartition.Offset / $sectorSize
$winPeRoot = "$winPeCheckpointLetter`:\Parq"
$request = [ordered]@{
    requestVersion                   = 2
    diskNumber                       = [int]$disk.Number
    expectedDiskSize                 = [long]$disk.Size
    expectedModel                    = $model
    expectedSerial                   = $serial
    expectedSourceSizeBefore         = [long]$sourcePartition.Size
    expectedSourceSizeAfter          = $ExpectedSourceSizeAfterBytes
    srcStartLba                      = $sourceStartLba
    newStartLba                      = [long]($NewStartBytes / $sectorSize)
    checkpointDiskNumber             = [int]$checkpointDisk.Number
    expectedCheckpointDiskSize       = [long]$checkpointDisk.Size
    expectedCheckpointModel          = $checkpointModel
    expectedCheckpointSerial         = $checkpointSerial
    checkpointPartitionStartLba      = $checkpointStartLba
    expectedCheckpointPartitionSize  = [long]$checkpointPartition.Size
    checkpointPath                   = "$winPeRoot\move-checkpoint.json"
    statePath                        = "$winPeRoot\offline-state.json"
}

if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $OutputPath = "$checkpointLetter`:\Parq\request.json"
}
$outputFullPath = [IO.Path]::GetFullPath($OutputPath)
$outputName = [IO.Path]::GetFileName($outputFullPath)
if (-not [string]::Equals($outputName, "request.json", [StringComparison]::OrdinalIgnoreCase)) {
    throw "OutputPath must end with request.json so the WinPE launcher can find it."
}
$outputRoot = [IO.Path]::GetPathRoot($outputFullPath)
if (-not [string]::Equals($outputRoot, "$checkpointLetter`:\", [StringComparison]::OrdinalIgnoreCase)) {
    throw "request.json must be stored on checkpoint volume $checkpointLetter`:."
}
$outputDirectory = Split-Path -Parent $outputFullPath
$stalePaths = @(
    $outputFullPath,
    (Join-Path $outputDirectory "move-checkpoint.json"),
    (Join-Path $outputDirectory "offline-state.json")
)
foreach ($stalePath in $stalePaths) {
    if (Test-Path -LiteralPath $stalePath) {
        throw "Refusing to overwrite an existing offline-move file: $stalePath"
    }
}
New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null
$json = $request | ConvertTo-Json -Depth 4
$utf8NoBom = [Text.UTF8Encoding]::new($false)
[IO.File]::WriteAllText($outputFullPath, $json, $utf8NoBom)

$identity = if ($null -ne $serial) { $serial } else { $model }
$confirmation = "MOVE WINDOWS $identity $sourceStartLba $($request.newStartLba)"
Write-Host "Offline move request created: $outputFullPath" -ForegroundColor Green
Write-Host "Source: $sourceLetter`: disk=$($disk.Number), LBA=$sourceStartLba, size=$($sourcePartition.Size)"
Write-Host "Plan: LBA $sourceStartLba -> $($request.newStartLba), size=$ExpectedSourceSizeAfterBytes"
Write-Host "Checkpoint: disk=$($checkpointDisk.Number), partition #$($checkpointPartition.PartitionNumber), WinPE $winPeCheckpointLetter`:"
Write-Host "Confirmation: $confirmation" -ForegroundColor Yellow
