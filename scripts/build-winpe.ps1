<#
.SYNOPSIS
  Parq 오프라인 시스템 이동 도구가 포함된 Windows PE ISO를 만든다.

.DESCRIPTION
  Windows ADK와 WinPE add-on의 copype/MakeWinPEMedia를 사용한다. WMI, StorageWMI,
  PowerShell, SecureStartup 구성 요소를 boot.wim에 추가하고 Parq 실행 파일과 런처를 복사한다.
  ISO 생성만 수행하며 USB 쓰기나 부팅 설정 변경은 하지 않는다.

.EXAMPLE
  .\scripts\build-winpe.ps1 -Architecture amd64

.EXAMPLE
  .\scripts\build-winpe.ps1 -Architecture arm64 `
    -OfflineBinaryPath C:\build\aarch64-pc-windows-msvc\release\examples\offline_system_move.exe
#>

[CmdletBinding()]
param(
    [ValidateSet("amd64", "arm64")]
    [string]$Architecture = $(if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "amd64" }),
    [string]$OfflineBinaryPath,
    [string]$OutputDirectory,
    [string]$AdkRoot = "${env:ProgramFiles(x86)}\Windows Kits\10\Assessment and Deployment Kit",
    [ValidatePattern('^[a-z]{2}-[A-Z]{2}$')]
    [string]$Locale = "ko-KR"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Run this script from an elevated PowerShell session."
    }
}

function Invoke-Checked {
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [Parameter(Mandatory)][string[]]$Arguments,
        [Parameter(Mandatory)][string]$Description
    )
    Write-Host "[$Description]" -ForegroundColor Cyan
    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed: exit=$LASTEXITCODE"
    }
}

function Get-PeMachine {
    param([Parameter(Mandatory)][string]$Path)
    $stream = [IO.File]::OpenRead($Path)
    try {
        if ($stream.Length -lt 64) {
            throw "The PE file is too small: $Path"
        }
        $reader = [IO.BinaryReader]::new($stream)
        if ($reader.ReadUInt16() -ne 0x5A4D) {
            throw "The executable has no MZ header: $Path"
        }
        $stream.Position = 0x3C
        $peOffset = $reader.ReadUInt32()
        if ($peOffset + 6 -gt $stream.Length) {
            throw "The PE header offset is invalid: $Path"
        }
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) {
            throw "The PE signature is invalid: $Path"
        }
        return $reader.ReadUInt16()
    }
    finally {
        $stream.Dispose()
    }
}

function Assert-StaticCrt {
    param([Parameter(Mandatory)][string]$Path)

    $imageText = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($Path))
    $dynamicCrt = [regex]::Match(
        $imageText,
        '(?i)VCRUNTIME[0-9_]*\.dll|MSVCP[0-9_]*\.dll|ucrtbase\.dll|api-ms-win-crt-[a-z0-9-]+\.dll'
    )
    if ($dynamicCrt.Success) {
        throw "Executable imports a C/C++ runtime that is not guaranteed in WinPE ($($dynamicCrt.Value)). Rebuild it with -C target-feature=+crt-static."
    }
}

function Import-VsBuildEnvironment {
    param([Parameter(Mandatory)][ValidateSet("amd64", "arm64")][string]$Target)

    $vsWhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vsWhere)) {
        throw "Visual Studio Build Tools were not found. Install the MSVC tools or supply -OfflineBinaryPath."
    }
    $component = if ($Target -eq "arm64") {
        "Microsoft.VisualStudio.Component.VC.Tools.ARM64"
    }
    else {
        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64"
    }
    $installation = @(& $vsWhere -latest -products * -requires $component -property installationPath)
    if ($LASTEXITCODE -ne 0 -or $installation.Count -ne 1) {
        throw "A Visual Studio installation with the $Target MSVC tools was not found."
    }
    $vcVarsAll = Join-Path $installation[0].Trim() "VC\Auxiliary\Build\vcvarsall.bat"
    if (-not (Test-Path -LiteralPath $vcVarsAll)) {
        throw "vcvarsall.bat was not found: $vcVarsAll"
    }

    $command = 'call "' + $vcVarsAll + '" ' + $Target + ' >nul && set'
    $environment = @(& cmd.exe /d /c $command)
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to initialize the Visual Studio $Target build environment."
    }
    foreach ($line in $environment) {
        $separator = $line.IndexOf('=')
        if ($separator -gt 0) {
            [Environment]::SetEnvironmentVariable(
                $line.Substring(0, $separator),
                $line.Substring($separator + 1),
                [EnvironmentVariableTarget]::Process
            )
        }
    }
    if ($null -eq (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        throw "The Visual Studio environment did not expose link.exe."
    }
}

function Add-WinPeOptionalComponent {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$OcRoot,
        [Parameter(Mandatory)][string]$MountPath,
        [Parameter(Mandatory)][string]$Language
    )
    $package = Join-Path $OcRoot "$Name.cab"
    if (-not (Test-Path -LiteralPath $package)) {
        throw "WinPE optional component not found: $package"
    }
    Invoke-Checked -FilePath "dism.exe" `
        -Arguments @("/Image:$MountPath", "/Add-Package", "/PackagePath:$package") `
        -Description "Add $Name"

    $languagePackage = Join-Path (Join-Path $OcRoot $Language) "$Name`_$Language.cab"
    if (Test-Path -LiteralPath $languagePackage) {
        Invoke-Checked -FilePath "dism.exe" `
            -Arguments @("/Image:$MountPath", "/Add-Package", "/PackagePath:$languagePackage") `
            -Description "Add $Name $Language language pack"
    }
}

Assert-Administrator

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$winPeRoot = Join-Path $AdkRoot "Windows Preinstallation Environment"
$deploymentTools = Join-Path $AdkRoot "Deployment Tools"
$copyPe = Join-Path $winPeRoot "copype.cmd"
$makeWinPeMedia = Join-Path $winPeRoot "MakeWinPEMedia.cmd"
$ocRoot = Join-Path (Join-Path $winPeRoot $Architecture) "WinPE_OCs"
$preferredHostTools = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "amd64" }
$hostToolsRoot = Join-Path $deploymentTools $preferredHostTools
if (-not (Test-Path -LiteralPath $hostToolsRoot)) {
    $hostToolsRoot = Join-Path $deploymentTools "amd64"
}
$dismRoot = Join-Path $hostToolsRoot "DISM"
$bcdBootRoot = Join-Path $hostToolsRoot "BCDBoot"
$oscdImgRoot = Join-Path $hostToolsRoot "Oscdimg"
$oscdImg = Join-Path $oscdImgRoot "oscdimg.exe"
foreach ($requiredPath in @($copyPe, $makeWinPeMedia, $ocRoot, $dismRoot, $bcdBootRoot, $oscdImg)) {
    if (-not (Test-Path -LiteralPath $requiredPath)) {
        throw "Windows ADK/WinPE add-on file not found: $requiredPath"
    }
}
$env:WinPERoot = $winPeRoot
$env:DeploymentTools = $deploymentTools
$env:DISMRoot = $dismRoot
$env:BCDBootRoot = $bcdBootRoot
$env:OSCDImgRoot = $oscdImgRoot
$env:PATH = "$dismRoot;$bcdBootRoot;$oscdImgRoot;$env:PATH"

if ([string]::IsNullOrWhiteSpace($OfflineBinaryPath)) {
    $rustBin = Join-Path $env:USERPROFILE ".cargo\bin"
    $rustcPath = Join-Path $rustBin "rustc.exe"
    $cargoPath = Join-Path $rustBin "cargo.exe"
    if (-not (Test-Path -LiteralPath $rustcPath)) {
        $rustcPath = (Get-Command rustc -ErrorAction Stop).Source
    }
    if (-not (Test-Path -LiteralPath $cargoPath)) {
        $cargoPath = (Get-Command cargo -ErrorAction Stop).Source
    }
    $rustHost = (& $rustcPath -vV | Select-String '^host:' | ForEach-Object {
        $_.Line.Substring(5).Trim()
    })
    $requiredHostPrefix = if ($Architecture -eq "arm64") { "aarch64-" } else { "x86_64-" }
    if (-not $rustHost.StartsWith($requiredHostPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Rust host $rustHost cannot build a $Architecture WinPE binary. Supply a matching build with -OfflineBinaryPath."
    }
    Import-VsBuildEnvironment -Target $Architecture
    $previousRustFlags = $env:RUSTFLAGS
    $previousRustc = $env:RUSTC
    $env:RUSTFLAGS = (($previousRustFlags, "-C target-feature=+crt-static") |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) }) -join " "
    $env:RUSTC = $rustcPath
    try {
        Invoke-Checked -FilePath $cargoPath `
            -Arguments @(
                "build", "--release", "--target", $rustHost,
                "--manifest-path", (Join-Path $repoRoot "src-tauri\Cargo.toml"),
                "--example", "offline_system_move"
            ) `
            -Description "Build static-CRT offline_system_move release binary"
    }
    finally {
        $env:RUSTFLAGS = $previousRustFlags
        $env:RUSTC = $previousRustc
    }
    $OfflineBinaryPath = Join-Path $repoRoot "src-tauri\target\$rustHost\release\examples\offline_system_move.exe"
}
$offlineBinary = (Resolve-Path -LiteralPath $OfflineBinaryPath).Path
$expectedMachine = if ($Architecture -eq "arm64") { 0xAA64 } else { 0x8664 }
$actualMachine = Get-PeMachine -Path $offlineBinary
if ($actualMachine -ne $expectedMachine) {
    throw ("Executable architecture does not match WinPE: actual=0x{0:X4}, expected=0x{1:X4}" -f $actualMachine, $expectedMachine)
}
Assert-StaticCrt -Path $offlineBinary

if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $OutputDirectory = Join-Path $repoRoot "artifacts\winpe-$Architecture-$stamp"
}
$outputFullPath = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $outputFullPath) {
    throw "Output directory already exists; choose a new path: $outputFullPath"
}
New-Item -ItemType Directory -Path $outputFullPath | Out-Null
$workingDirectory = Join-Path $outputFullPath "media-work"
$isoPath = Join-Path $outputFullPath "Parq-WinPE-$Architecture.iso"

$mounted = $false
$committed = $false
try {
    Invoke-Checked -FilePath $copyPe -Arguments @($Architecture, $workingDirectory) `
        -Description "Create WinPE working directory"
    $bootWim = Join-Path $workingDirectory "media\sources\boot.wim"
    $mountPath = Join-Path $workingDirectory "mount"
    Invoke-Checked -FilePath "dism.exe" `
        -Arguments @("/Mount-Image", "/ImageFile:$bootWim", "/Index:1", "/MountDir:$mountPath") `
        -Description "Mount boot.wim"
    $mounted = $true

    foreach ($component in @(
        "WinPE-WMI", "WinPE-NetFX", "WinPE-Scripting", "WinPE-PowerShell",
        "WinPE-StorageWMI", "WinPE-SecureStartup"
    )) {
        Add-WinPeOptionalComponent -Name $component -OcRoot $ocRoot `
            -MountPath $mountPath -Language $Locale
    }
    Invoke-Checked -FilePath "dism.exe" `
        -Arguments @("/Image:$mountPath", "/Set-ScratchSpace:512") `
        -Description "Set WinPE scratch space"

    $parqDirectory = Join-Path $mountPath "Parq"
    New-Item -ItemType Directory -Path $parqDirectory | Out-Null
    Copy-Item -LiteralPath $offlineBinary `
        -Destination (Join-Path $parqDirectory "offline_system_move.exe")
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot "winpe\Invoke-ParqOfflineMove.ps1") `
        -Destination $parqDirectory
    Copy-Item -LiteralPath (Join-Path $repoRoot "LICENSE.MD") -Destination $parqDirectory
    Copy-Item -LiteralPath (Join-Path $repoRoot "docs\winpe-offline-system-move.md") `
        -Destination (Join-Path $parqDirectory "README.md")

    $startnetPath = Join-Path $mountPath "Windows\System32\startnet.cmd"
    $startnet = @(
        '@echo off'
        'chcp 65001 >nul'
        'wpeinit'
        'echo.'
        'echo Parq offline system move environment'
        'echo The launcher will only execute after an exact confirmation phrase.'
        'powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File X:\Parq\Invoke-ParqOfflineMove.ps1'
        'echo.'
        'echo Launcher exited. Review the error above or run it again from X:\Parq.'
        'cmd.exe'
    ) -join "`r`n"
    [IO.File]::WriteAllText($startnetPath, $startnet, [Text.ASCIIEncoding]::new())

    Invoke-Checked -FilePath "dism.exe" `
        -Arguments @("/Unmount-Image", "/MountDir:$mountPath", "/Commit") `
        -Description "Commit boot.wim"
    $mounted = $false
    $committed = $true
    Invoke-Checked -FilePath $makeWinPeMedia `
        -Arguments @("/ISO", $workingDirectory, $isoPath) `
        -Description "Create WinPE ISO"
}
finally {
    if ($mounted -and -not $committed) {
        Write-Warning "Discarding the mounted WIM after an error."
        & dism.exe "/Unmount-Image" "/MountDir:$mountPath" "/Discard"
    }
}

$isoHash = Get-FileHash -LiteralPath $isoPath -Algorithm SHA256
Write-Host "WinPE ISO created: $isoPath" -ForegroundColor Green
Write-Host "SHA256: $($isoHash.Hash)"
Write-Host "The script created an ISO only; it did not change VM boot settings."
