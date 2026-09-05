# denia 自举开发脚本:构建开发实例 denia_develop.exe 并起在独立端口,
# 全程不动正在运行的正式 denia.exe,实现"denia 自己改自己"。
#
#   pwsh scripts/dev.ps1            构建 + 起开发实例(前台)
#   pwsh scripts/dev.ps1 -Bg        构建 + 后台起开发实例
#   pwsh scripts/dev.ps1 -BuildOnly 只构建,不起服务
#
# 环境变量覆盖:
#   DEV_PORT   开发实例端口(默认 3601)
#   DEV_HOME   开发实例数据目录(默认跟随主实例:~/.denia;旧目录 ~/.dsh-rs 由
#              服务端启动时一次性整体迁移过去,两边仍共享模型配置/会话;
#              想完全隔离可显式指定其他目录)
[CmdletBinding()]
param(
  [switch]$Bg,
  [switch]$BuildOnly
)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)

$DevPort = if ($env:DEV_PORT) { $env:DEV_PORT } else { 3601 }
# 与 main.rs resolve_home 同规则:数据目录始终 ~/.denia(旧 ~/.dsh-rs 由服务端
# 启动时自动迁移)。
$DevHome = if ($env:DEV_HOME) { $env:DEV_HOME } else { Join-Path $HOME '.denia' }

Write-Host "`n==> building web console"
if (-not (Test-Path 'web/node_modules')) {
  Push-Location web
  try { pnpm install; if ($LASTEXITCODE -ne 0) { throw '前端依赖安装失败' } } finally { Pop-Location }
}
Push-Location web
try { pnpm build; if ($LASTEXITCODE -ne 0) { throw '前端构建失败' } } finally { Pop-Location }

Write-Host "`n==> release build (no install, no kill)"
cargo build --release -p denia-server
if ($LASTEXITCODE -ne 0) { throw '后端构建失败，保留现有开发实例' }

Write-Host "`n==> staging denia_develop"
# 只杀旧开发实例(正式 denia.exe 绝不碰),再复制替换。
# target/release/denia.exe 无人运行,复制不受 Windows 文件锁影响。
Get-Process denia_develop -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
Copy-Item -Force 'target/release/denia.exe' 'denia_develop.exe'
$DevExe = Join-Path $PWD 'denia_develop.exe'

if ($BuildOnly) {
  Write-Host "`n==> done (build only)"
  Write-Host "dev binary: $DevExe"
  exit 0
}

$DistDir = Join-Path $PWD 'web/dist'
Write-Host "`n==> starting dev instance"
Write-Host "  console:  http://127.0.0.1:$DevPort"
Write-Host "  home:     $DevHome"
Write-Host "  web dist: $DistDir (从磁盘读取;pnpm build 后刷新即生效)"

if ($Bg) {
  Start-Process -FilePath $DevExe `
    -ArgumentList '--port', "$DevPort", '--home', $DevHome, '--web', $DistDir `
    -WindowStyle Hidden
  Write-Host "dev instance started in background — http://127.0.0.1:$DevPort/"
} else {
  & $DevExe --port $DevPort --home $DevHome --web $DistDir
}
