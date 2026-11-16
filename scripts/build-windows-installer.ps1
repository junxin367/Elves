param(
    [string]$Version = "",
    [string]$MakensisPath = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Resolve-RepoRoot {
    $scriptDir = Split-Path -Parent $PSCommandPath
    return (Resolve-Path -LiteralPath (Join-Path $scriptDir "..")).Path
}

function Resolve-WorkspaceVersion {
    param([string]$RepoRoot)

    if ($Version.Trim()) {
        return $Version.Trim()
    }

    $cargoToml = Join-Path $RepoRoot "Cargo.toml"
    $content = Get-Content -LiteralPath $cargoToml -Raw
    $match = [regex]::Match($content, '(?m)^\s*version\s*=\s*"([^"]+)"')
    if (!$match.Success) {
        throw "Failed to read workspace version from Cargo.toml"
    }
    return $match.Groups[1].Value
}

function Resolve-ToolPath {
    param(
        [string]$Name,
        [string]$ExplicitPath,
        [string[]]$FallbackPaths
    )

    if ($ExplicitPath.Trim()) {
        if (!(Test-Path -LiteralPath $ExplicitPath)) {
            throw "Configured $Name path does not exist: $ExplicitPath"
        }
        return (Resolve-Path -LiteralPath $ExplicitPath).Path
    }

    # 多套 Node/npm 并存时按 PATH 顺序选择一个可执行文件，不能把路径数组传给构建命令。
    $command = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -ne $command) {
        return $command.Path
    }

    foreach ($fallback in $FallbackPaths) {
        if (Test-Path -LiteralPath $fallback) {
            return (Resolve-Path -LiteralPath $fallback).Path
        }
    }

    throw "Unable to find $Name. Install it or pass -MakensisPath for NSIS."
}

function Invoke-Checked {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [string]$WorkingDirectory
    )

    Push-Location $WorkingDirectory
    try {
        & $FilePath @Arguments
        if ($LASTEXITCODE -ne 0) {
            throw "Command failed with exit code ${LASTEXITCODE}: $FilePath $($Arguments -join ' ')"
        }
    }
    finally {
        Pop-Location
    }
}

function Start-CargoCacheCleanup {
    param(
        [string]$RepoRoot,
        [double]$MaxSizeGiB
    )

    if ($env:CODEX_ELVES_SKIP_CARGO_CACHE_CLEANUP -eq "1") {
        Write-Output "[INFO] Cargo cache cleanup skipped by environment"
        return
    }

    $cleanupScript = Join-Path $RepoRoot "scripts\cleanup-cargo-cache.ps1"
    if (!(Test-Path -LiteralPath $cleanupScript)) {
        Write-Warning "Cargo cache cleanup script not found: $cleanupScript"
        return
    }

    try {
        & $cleanupScript -RepoRoot $RepoRoot -MaxSizeGiB $MaxSizeGiB -Background
    }
    catch {
        Write-Warning "Unable to schedule Cargo cache cleanup: $($_.Exception.Message)"
    }
}

$RepoRoot = Resolve-RepoRoot
$ResolvedVersion = Resolve-WorkspaceVersion -RepoRoot $RepoRoot
$ManagementDir = Join-Path $RepoRoot "apps\elves-management"
$DistAppDir = Join-Path $RepoRoot "dist\windows\app"
$InstallerScriptDir = Join-Path $RepoRoot "scripts\installer\windows"
$InstallerScript = Join-Path $InstallerScriptDir "Elves.nsi"
$InstallerPath = Join-Path $RepoRoot "dist\windows\Elves-$ResolvedVersion-windows-x64-setup.exe"
if ($env:CARGO_TARGET_DIR) {
    if ([System.IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
        $CargoTargetDir = [System.IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
    }
    else {
        $CargoTargetDir = [System.IO.Path]::GetFullPath(
            (Join-Path $ManagementDir $env:CARGO_TARGET_DIR)
        )
    }
}
else {
    $CargoTargetDir = Join-Path $RepoRoot "target"
}
$ReleaseDir = Join-Path $CargoTargetDir "release-fast"
$ReleaseMirrorDir = Join-Path $CargoTargetDir "release"

$NpmPath = Resolve-ToolPath -Name "npm.cmd" -ExplicitPath "" -FallbackPaths @()
$NodePath = Resolve-ToolPath -Name "node.exe" -ExplicitPath "" -FallbackPaths @()
$NsisPath = Resolve-ToolPath `
    -Name "makensis" `
    -ExplicitPath $MakensisPath `
    -FallbackPaths @(
        "C:\Program Files\NSIS\makensis.exe",
        "C:\Program Files\NSIS\Bin\makensis.exe",
        "C:\Program Files (x86)\NSIS\makensis.exe",
        "C:\Program Files (x86)\NSIS\Bin\makensis.exe"
    )

Write-Output "[INFO] Repo: $RepoRoot"
Write-Output "[INFO] Version: $ResolvedVersion"
Write-Output "[INFO] npm: $NpmPath"
Write-Output "[INFO] makensis: $NsisPath"
Write-Output "[INFO] Cargo target: $CargoTargetDir"

$BuildInputVerifier = Join-Path $RepoRoot "scripts\verify-windows-build-inputs.mjs"
$ToolRuntimeVerifier = Join-Path $RepoRoot "scripts\verify-tool-runtime.mjs"
# 用户要求已修复功能不能因构建基线回退；编译前核对服务协议及压缩恢复，复制前后再核对完整资源。
Invoke-Checked -FilePath $NodePath -Arguments @($BuildInputVerifier, $RepoRoot, "--source-only") -WorkingDirectory $RepoRoot
Invoke-Checked -FilePath $NodePath -Arguments @($ToolRuntimeVerifier, $RepoRoot, "--source-only") -WorkingDirectory $RepoRoot
Invoke-Checked -FilePath $NpmPath -Arguments @("run", "build") -WorkingDirectory $ManagementDir
Invoke-Checked -FilePath $NodePath -Arguments @($BuildInputVerifier, $RepoRoot, $ReleaseDir) -WorkingDirectory $RepoRoot

New-Item -ItemType Directory -Path $DistAppDir -Force | Out-Null
New-Item -ItemType Directory -Path $ReleaseMirrorDir -Force | Out-Null
$ClientExe = Join-Path $ReleaseDir "elves.exe"

# 用户要求只安装一个 Elves 程序；后台与管理功能已内置该 EXE，不再构建或打包独立管理器。
foreach ($obsoleteName in @("codex-elves.exe", "codex-elves-manager.exe", "codex-elves-task-board.exe", "codex-elves-remote.exe", "elves-management.exe")) {
    Remove-Item -LiteralPath (Join-Path $DistAppDir $obsoleteName) -Force -ErrorAction SilentlyContinue
}
# 旧版构建目录可能留下分散工具；它们已内嵌客户端，安装输入只保留单个程序。
$obsoleteTools = Join-Path $DistAppDir "tools"
if (Test-Path -LiteralPath $obsoleteTools) {
    $resolvedTools = (Resolve-Path -LiteralPath $obsoleteTools).Path
    $resolvedApp = (Resolve-Path -LiteralPath $DistAppDir).Path.TrimEnd('\') + '\'
    if (-not $resolvedTools.StartsWith($resolvedApp, [StringComparison]::OrdinalIgnoreCase)) {
        throw "旧工具目录不在安装输入目录内：$resolvedTools"
    }
    Remove-Item -LiteralPath $resolvedTools -Recurse -Force
}
foreach ($artifact in @($ClientExe)) {
    if (!(Test-Path -LiteralPath $artifact)) {
        throw "Expected release artifact not found: $artifact"
    }
    # target/release 中的 exe 与 deps 产物是硬链接，需先删除再复制，避免改写 cargo 缓存
    $mirrorPath = Join-Path $ReleaseMirrorDir (Split-Path -Leaf $artifact)
    Remove-Item -LiteralPath $mirrorPath -Force -ErrorAction SilentlyContinue
    Copy-Item -LiteralPath $artifact -Destination $mirrorPath -Force
    Copy-Item -LiteralPath $artifact -Destination $DistAppDir -Force
}

# 单客户端须自带本机后台的工具，安装输入不再附带独立工具程序。
Invoke-Checked -FilePath $NodePath -Arguments @($ToolRuntimeVerifier, $RepoRoot, "--embedded", $ClientExe) -WorkingDirectory $RepoRoot
Invoke-Checked -FilePath $NodePath -Arguments @($ToolRuntimeVerifier, $RepoRoot, "--embedded", (Join-Path $DistAppDir "elves.exe")) -WorkingDirectory $RepoRoot
Invoke-Checked -FilePath $NodePath -Arguments @($BuildInputVerifier, $RepoRoot, $DistAppDir) -WorkingDirectory $RepoRoot

# 校验独立客户端为 x64 GUI 程序，并保留单独交付的便携 EXE；安装包同时嵌入上述同一份客户端。
& (Join-Path $RepoRoot "scripts\build-windows-client.ps1") `
    -Version $ResolvedVersion -Profile "release-fast" -SkipBuild -BinaryDirectory $ReleaseDir

Invoke-Checked `
    -FilePath $NsisPath `
    -Arguments @("/INPUTCHARSET", "UTF8", "/DVERSION=$ResolvedVersion", $InstallerScript) `
    -WorkingDirectory $InstallerScriptDir

if (!(Test-Path -LiteralPath $InstallerPath)) {
    throw "Installer was not created: $InstallerPath"
}

$InstallerItem = Get-Item -LiteralPath $InstallerPath
$Length = $InstallerItem.Length
$LastWriteTime = $InstallerItem.LastWriteTime
Write-Output "[OK] Installer: $InstallerPath"
Write-Output "[OK] Size: $Length bytes"
Write-Output "[OK] Updated: $LastWriteTime"

Start-CargoCacheCleanup -RepoRoot $RepoRoot -MaxSizeGiB 20
