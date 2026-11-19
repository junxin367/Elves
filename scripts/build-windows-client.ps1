param(
    [string]$Version = "",
    [ValidateSet("release-fast", "release")]
    [string]$Profile = "release-fast",
    [switch]$SkipBuild,
    [string]$BinaryDirectory = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$RepoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..")).Path
$workspace = Get-Content -LiteralPath (Join-Path $RepoRoot "Cargo.toml") -Raw
$versionMatch = [regex]::Match($workspace, '(?m)^\s*version\s*=\s*"([^"]+)"')
if (!$versionMatch.Success) { throw "Unable to resolve workspace version." }
$WorkspaceVersion = $versionMatch.Groups[1].Value
$ResolvedVersion = if ($Version.Trim()) { $Version.Trim() } else { $WorkspaceVersion }
if ($ResolvedVersion -notmatch '^\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?$') {
    throw "Invalid artifact version: $ResolvedVersion"
}
if ($BinaryDirectory -and !$SkipBuild) {
    throw "-BinaryDirectory is only supported with -SkipBuild."
}

function Remove-StaleClientBackups {
    param(
        [string]$Directory,
        [string]$Destination
    )

    $directoryRoot = [System.IO.Path]::GetFullPath($Directory).TrimEnd('\') + '\'
    $destinationPath = [System.IO.Path]::GetFullPath($Destination)
    if (!$destinationPath.StartsWith($directoryRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw "客户端备份目标不在构建产物目录内：$destinationPath"
    }
    $pattern = "$([System.IO.Path]::GetFileName($destinationPath)).previous-*"
    $backups = @(Get-ChildItem -LiteralPath $Directory -Filter $pattern -File -ErrorAction SilentlyContinue)
    foreach ($backup in $backups) {
        try {
            Remove-Item -LiteralPath $backup.FullName -Force -ErrorAction Stop
            Write-Output "[INFO] Removed stale client backup: $($backup.Name)"
        }
        catch {
            # 用户可能仍在运行旧版便携客户端；保留被占用的备份，下一次构建继续重试清理。
            Write-Output "[INFO] Preserved in-use client backup: $($backup.Name)"
        }
    }
}

# 用户要求只提供一个 Elves Windows 程序；此入口只构建客户端，不启动管理器、不构建 Android。
if (!$SkipBuild) {
    $cargo = Get-Command "cargo.exe" -CommandType Application -ErrorAction Stop | Select-Object -First 1
    $npm = Get-Command "npm.cmd" -CommandType Application -ErrorAction Stop | Select-Object -First 1
    Push-Location $RepoRoot
    try {
        # 用户要求缩短日常编译并保持资源正确：检查始终执行，前端仅在输入或产物摘要变化时重建。
        # 统一在 Rust 前准备管理资源，避免单独编译客户端时内嵌到旧页面。
        $checkClock = [Diagnostics.Stopwatch]::StartNew()
        & $npm.Path --prefix apps/elves-management run check:harness
        if ($LASTEXITCODE -ne 0) { throw "Harness resource check failed: $LASTEXITCODE" }
        Write-Output "[TIME] Harness check: $([math]::Round($checkClock.Elapsed.TotalSeconds, 2)) s"
        $frontendClock = [Diagnostics.Stopwatch]::StartNew()
        & $npm.Path --prefix apps/elves-management run build:frontend
        if ($LASTEXITCODE -ne 0) { throw "Management frontend build failed: $LASTEXITCODE" }
        Write-Output "[TIME] Management frontend: $([math]::Round($frontendClock.Elapsed.TotalSeconds, 2)) s"
        $compileClock = [Diagnostics.Stopwatch]::StartNew()
        & $cargo.Path build --locked --profile $Profile -p elves --bin elves
        if ($LASTEXITCODE -ne 0) { throw "Elves client build failed: $LASTEXITCODE" }
        Write-Output "[TIME] Cargo ($Profile): $([math]::Round($compileClock.Elapsed.TotalSeconds, 2)) s"
    }
    finally { Pop-Location }
}

if ($BinaryDirectory) {
    $BuildDirectory = [System.IO.Path]::GetFullPath(
        $(if ([System.IO.Path]::IsPathRooted($BinaryDirectory)) { $BinaryDirectory } else { Join-Path $RepoRoot $BinaryDirectory })
    )
}
else {
    $targetRoot = if ($env:CARGO_TARGET_DIR) {
        [System.IO.Path]::GetFullPath(
            $(if ([System.IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) { $env:CARGO_TARGET_DIR } else { Join-Path $RepoRoot $env:CARGO_TARGET_DIR })
        )
    } else { Join-Path $RepoRoot "target" }
    if ($env:CARGO_BUILD_TARGET) { $targetRoot = Join-Path $targetRoot $env:CARGO_BUILD_TARGET }
    $BuildDirectory = Join-Path $targetRoot $Profile
}
$Executable = Join-Path $BuildDirectory "elves.exe"
if (!(Test-Path -LiteralPath $Executable -PathType Leaf)) {
    throw "Elves client executable was not created: $Executable"
}

# 校验实际 PE 机器类型与 GUI 子系统，不能把其他目标产物命名为 Windows x64 或带控制台的客户端。
$image = [System.IO.File]::ReadAllBytes($Executable)
if ($image.Length -lt 256 -or $image[0] -ne 0x4d -or $image[1] -ne 0x5a) { throw "Invalid PE executable." }
$peOffset = [BitConverter]::ToInt32($image, 0x3c)
if ($peOffset -lt 0 -or $peOffset + 96 -ge $image.Length) { throw "Invalid PE header offset." }
if ([BitConverter]::ToUInt32($image, $peOffset) -ne 0x4550 -or
    [BitConverter]::ToUInt16($image, $peOffset + 4) -ne 0x8664 -or
    [BitConverter]::ToUInt16($image, $peOffset + 24 + 68) -ne 2) {
    throw "Elves client must be a Windows x64 GUI executable."
}
$DistDirectory = Join-Path $RepoRoot "dist\windows"
New-Item -ItemType Directory -Path $DistDirectory -Force | Out-Null
$Destination = Join-Path $DistDirectory "Elves-$ResolvedVersion-windows-x64.exe"
# 用户要求构建不长期积累 previous 备份；未被占用的历史文件在替换前先清理。
Remove-StaleClientBackups -Directory $DistDirectory -Destination $Destination
$sourceHasher = [Security.Cryptography.SHA256]::Create()
try { $sourceDigest = [BitConverter]::ToString($sourceHasher.ComputeHash($image)).Replace("-", "").ToLowerInvariant() }
finally { $sourceHasher.Dispose() }
# 用户可能正在使用便携客户端；相同字节无需覆盖，避免常规安装包编译因运行中的同版 EXE 被锁定而失败。
if (!(Test-Path -LiteralPath $Destination -PathType Leaf) -or
    (Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash.ToLowerInvariant() -ne $sourceDigest) {
    # 编译时保留正在使用的客户端：先写入完整新文件，再轮换旧产物，不停止用户窗口。
    $artifactSuffix = [Guid]::NewGuid().ToString("N")
    $temporaryArtifact = Join-Path $DistDirectory "remote-$artifactSuffix.tmp"
    $previousArtifact = "$Destination.previous-$artifactSuffix"
    [IO.File]::WriteAllBytes($temporaryArtifact, $image)
    try {
        if (Test-Path -LiteralPath $Destination -PathType Leaf) {
            Move-Item -LiteralPath $Destination -Destination $previousArtifact -ErrorAction Stop
        }
        try { Move-Item -LiteralPath $temporaryArtifact -Destination $Destination -ErrorAction Stop }
        catch {
            if (!(Test-Path -LiteralPath $Destination) -and (Test-Path -LiteralPath $previousArtifact)) {
                Move-Item -LiteralPath $previousArtifact -Destination $Destination -ErrorAction Stop
            }
            throw
        }
    }
    finally { Remove-Item -LiteralPath $temporaryArtifact -Force -ErrorAction SilentlyContinue }
}
# 替换完成后再次清理；如果旧客户端仍占用文件，保留它直到后续构建重试。
Remove-StaleClientBackups -Directory $DistDirectory -Destination $Destination
$digest = (Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash.ToLowerInvariant()
if ($digest -ne $sourceDigest) { throw "Staged Elves client does not match the validated executable." }
$info = [ordered]@{
    product = "Elves"
    version = $WorkspaceVersion
    artifactVersion = $ResolvedVersion
    platform = "windows-x64"
    profile = $Profile
    executable = [System.IO.Path]::GetFileName($Destination)
    bytes = (Get-Item -LiteralPath $Destination).Length
    sha256 = $digest
    builtAt = [DateTimeOffset]::Now.ToString("o")
}
$InfoPath = Join-Path $DistDirectory "Elves-$ResolvedVersion-windows-x64-build-info.json"
$text = ($info | ConvertTo-Json -Depth 4) + "`n"
[System.IO.File]::WriteAllText($InfoPath, $text, [System.Text.UTF8Encoding]::new($false))
Write-Output "[OK] Elves client: $Destination"
Write-Output "[OK] SHA256: $digest"
Write-Output "[OK] Build info: $InfoPath"
