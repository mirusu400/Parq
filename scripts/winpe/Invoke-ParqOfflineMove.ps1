<#
.SYNOPSIS
  Parq WinPE 오프라인 이동 런처.

.DESCRIPTION
  WinPE에서 request.json을 찾고, 요청에 기록된 시작 LBA로 원본/체크포인트 파티션을 다시
  식별한 뒤 임시 드라이브 문자를 부여한다. Preflight는 디스크 데이터를 쓰지 않으며,
  Execute만 offline_system_move.exe의 실제 이동 경로를 호출한다.
#>

[CmdletBinding()]
param(
    [ValidateSet("Interactive", "Preflight", "Execute")]
    [string]$Action = "Interactive",
    [string]$RequestPath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Find-RequestPath {
    foreach ($code in @(65..87) + @(89..90)) {
        $letter = [char]$code
        $candidate = "$letter`:\Parq\request.json"
        if (Test-Path -LiteralPath $candidate) {
            return $candidate
        }
    }
    throw "No \Parq\request.json was found on drives A:-W: or Y:-Z:."
}

function Get-RequestLetter {
    param([Parameter(Mandatory)][string]$Path)
    if ($Path -notmatch '^([A-WY-Za-wy-z]):\\') {
        throw "Checkpoint/state paths must be absolute drive paths other than X: ($Path)."
    }
    return $Matches[1].ToUpperInvariant()
}

function Get-FreeDriveLetter {
    param([string[]]$Reserved)
    $used = @(Get-Volume | Where-Object DriveLetter | ForEach-Object {
        ([string]$_.DriveLetter).ToUpperInvariant()
    })
    foreach ($letter in @("W", "V", "U", "T", "S")) {
        if ($letter -notin $used -and $letter -notin $Reserved) {
            return $letter
        }
    }
    throw "No free drive letter is available for the source volume."
}

function Set-VerifiedDriveLetter {
    param(
        [Parameter(Mandatory)][object]$Partition,
        [Parameter(Mandatory)][string]$Letter,
        [Parameter(Mandatory)][string]$Description
    )

    $current = [string]$Partition.DriveLetter
    if ($current -and $current[0] -ne [char]0 -and $current -eq $Letter) {
        return
    }
    $occupied = @(Get-Partition -DriveLetter $Letter -ErrorAction SilentlyContinue)
    if ($occupied.Count -gt 0) {
        throw "$Letter`: is already used and cannot be assigned to $Description."
    }
    Set-Partition -DiskNumber $Partition.DiskNumber `
        -PartitionNumber $Partition.PartitionNumber -NewDriveLetter $Letter -ErrorAction Stop
    $verify = Get-Partition -DriveLetter $Letter -ErrorAction Stop
    if ($verify.DiskNumber -ne $Partition.DiskNumber -or
        $verify.PartitionNumber -ne $Partition.PartitionNumber) {
        throw "$Description drive-letter verification failed."
    }
}

if ([string]::IsNullOrWhiteSpace($RequestPath)) {
    $RequestPath = Find-RequestPath
}
$RequestPath = (Resolve-Path -LiteralPath $RequestPath).Path
$request = Get-Content -Raw -LiteralPath $RequestPath -Encoding UTF8 | ConvertFrom-Json

$required = @(
    "diskNumber", "expectedDiskSize", "expectedModel", "expectedSourceSizeBefore",
    "expectedSourceSizeAfter", "srcStartLba", "newStartLba",
    "checkpointPartitionStartLba", "checkpointPath", "statePath"
)
foreach ($name in $required) {
    if ($null -eq $request.PSObject.Properties[$name]) {
        throw "request.json is missing required field: $name"
    }
}

$checkpointLetter = Get-RequestLetter -Path ([string]$request.checkpointPath)
$stateLetter = Get-RequestLetter -Path ([string]$request.statePath)
if ($checkpointLetter -ne $stateLetter) {
    throw "checkpointPath and statePath must use the same drive."
}

$disk = Get-Disk -Number ([int]$request.diskNumber) -ErrorAction Stop
if ([long]$disk.Size -ne [long]$request.expectedDiskSize) {
    throw "Target disk size differs from request.json."
}
if ([string]$disk.PartitionStyle -ne "GPT" -or $disk.IsReadOnly) {
    throw "The target must be a writable GPT disk."
}
$rawDisk = @(Get-CimInstance -Namespace "ROOT\Microsoft\Windows\Storage" -ClassName MSFT_Disk |
    Where-Object { $_.Number -eq $disk.Number })
if ($rawDisk.Count -ne 1) {
    throw "Exactly one MSFT_Disk must match the requested disk number."
}
$actualModel = ([string]$rawDisk[0].Model).Trim()
if ([string]::IsNullOrWhiteSpace($actualModel)) {
    $actualModel = "Unknown"
}
$actualSerial = ([string]$rawDisk[0].SerialNumber).Trim()
$expectedSerial = [string]$request.expectedSerial
if ($actualModel -cne [string]$request.expectedModel -or
    $actualSerial -cne $expectedSerial) {
    throw "Target disk model/serial fingerprint differs from request.json."
}
$sectorSize = [long]$disk.LogicalSectorSize
$sourceOffset = [decimal]$request.srcStartLba * [decimal]$sectorSize
$newOffset = [decimal]$request.newStartLba * [decimal]$sectorSize
$checkpointOffset = [decimal]$request.checkpointPartitionStartLba * [decimal]$sectorSize
$partitions = @(Get-Partition -DiskNumber $disk.Number)
$source = @($partitions | Where-Object {
    [long]$_.Offset -eq $sourceOffset -or [long]$_.Offset -eq $newOffset
})
if ($source.Count -ne 1) {
    throw "Exactly one partition must match the requested old/new start LBA."
}
$checkpoint = @($partitions | Where-Object { [long]$_.Offset -eq $checkpointOffset })
if ($checkpoint.Count -ne 1) {
    throw "Exactly one partition must match the checkpoint start LBA."
}
if ($source[0].PartitionNumber -eq $checkpoint[0].PartitionNumber) {
    throw "Source and checkpoint resolve to the same partition."
}

Set-VerifiedDriveLetter -Partition $checkpoint[0] -Letter $checkpointLetter `
    -Description "checkpoint volume"
$sourceLetter = [string]$source[0].DriveLetter
if ([string]::IsNullOrWhiteSpace($sourceLetter) -or $sourceLetter[0] -eq [char]0) {
    $sourceLetter = Get-FreeDriveLetter -Reserved @($checkpointLetter, "X")
    Set-VerifiedDriveLetter -Partition $source[0] -Letter $sourceLetter -Description "source volume"
}

$requestPathAfterMount = "$checkpointLetter`:\Parq\request.json"
if (-not (Test-Path -LiteralPath $requestPathAfterMount)) {
    throw "request.json is missing after mounting the checkpoint volume: $requestPathAfterMount"
}

$identity = if ($null -ne $request.expectedSerial -and
    -not [string]::IsNullOrWhiteSpace([string]$request.expectedSerial)) {
    [string]$request.expectedSerial
}
else {
    [string]$request.expectedModel
}
$expectedConfirmation = "MOVE WINDOWS $identity $($request.srcStartLba) $($request.newStartLba)"

Write-Host ""
Write-Host "Parq WinPE offline move" -ForegroundColor Cyan
Write-Host "  Request: $requestPathAfterMount"
Write-Host "  Disk: #$($disk.Number) $($disk.FriendlyName) ($($disk.Size) bytes)"
Write-Host "  Source: partition #$($source[0].PartitionNumber), $sourceLetter`:"
Write-Host "  Move: LBA $($request.srcStartLba) -> $($request.newStartLba)"
Write-Host "  Checkpoint: partition #$($checkpoint[0].PartitionNumber), $checkpointLetter`:"
Write-Host ""

if ($Action -eq "Interactive") {
    $currentSize = [long]$source[0].Size
    if ($currentSize -eq [long]$request.expectedSourceSizeAfter) {
        $choice = Read-Host "Source size is ready. [P]reflight / [E]xecute / [Q]uit"
    }
    elseif ($currentSize -eq [long]$request.expectedSourceSizeBefore) {
        Write-Host "The source is still at its before-shrink size. Run preflight, then return to Windows and shrink it to the planned size." -ForegroundColor Yellow
        $choice = Read-Host "[P]reflight / [Q]uit"
    }
    else {
        throw "Current source size matches neither the before nor after value in request.json."
    }
    switch ($choice.ToUpperInvariant()) {
        "P" { $Action = "Preflight" }
        "E" {
            if ($currentSize -ne [long]$request.expectedSourceSizeAfter) {
                throw "Execute requires the exact planned after-shrink size."
            }
            $Action = "Execute"
        }
        default { Write-Host "Cancelled."; return }
    }
}

$typed = Read-Host "Type this confirmation exactly: $expectedConfirmation"
if ($typed -cne $expectedConfirmation) {
    throw "Confirmation text does not match."
}
$env:PARQ_ENABLE_V2_DESTRUCTIVE = "1"
$env:PARQ_ENABLE_OFFLINE_SYSTEM_MOVE = "1"
$env:PARQ_OFFLINE_CONFIRMATION = $typed
[Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

$binary = "X:\Parq\offline_system_move.exe"
if (-not (Test-Path -LiteralPath $binary)) {
    throw "Offline move executable not found: $binary"
}
if ($Action -eq "Preflight") {
    & $binary --preflight $requestPathAfterMount
}
else {
    $current = Get-Partition -DiskNumber $disk.Number -PartitionNumber $source[0].PartitionNumber
    if ([long]$current.Size -ne [long]$request.expectedSourceSizeAfter) {
        throw "Source size changed before execution."
    }
    & $binary $requestPathAfterMount
}
if ($LASTEXITCODE -ne 0) {
    throw "offline_system_move.exe failed: exit=$LASTEXITCODE"
}
Write-Host "[PASS] $Action complete" -ForegroundColor Green
