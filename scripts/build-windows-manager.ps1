param(
    [ValidateSet("release-fast", "release")]
    [string]$Profile = "release-fast"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$RepoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..")).Path
$npm = Get-Command "npm.cmd" -CommandType Application -ErrorAction Stop | Select-Object -First 1
$cargo = Get-Command "cargo.exe" -CommandType Application -ErrorAction Stop | Select-Object -First 1

Push-Location $RepoRoot
try {
    # 独立管理器沿用原供应商、代理日志和会话管理页面；构建结果只供 Tauri 管理器使用。
    & $npm.Path --prefix apps/elves-management run build:manager
    if ($LASTEXITCODE -ne 0) { throw "代理管理器前端构建失败：$LASTEXITCODE" }

    & $cargo.Path build --locked --profile $Profile -p elves-manager --bin elves-manager
    if ($LASTEXITCODE -ne 0) { throw "代理管理器 Rust 构建失败：$LASTEXITCODE" }
}
finally {
    Pop-Location
}

Write-Output "[OK] Elves 代理管理器构建完成：target/$Profile/elves-manager.exe"
