param(
    [ValidateRange(1, 65535)]
    [int]$Port = 1420
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..")).Path
$managementDirectory = Join-Path $repoRoot "apps\elves-management"

# 管理页面只作为 Harness 共享前端开发，不再创建管理器程序或请求管理员权限。
npm --prefix $managementDirectory run dev -- --port $Port
$devExitCode = $LASTEXITCODE
exit $devExitCode
