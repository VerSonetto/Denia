# denia one-shot build (Windows): kill running instances, build web, install
# server, then start denia in the background so the console is ready to refresh.
#
#   scripts\build.ps1           build + install + start background server
#   scripts\build.ps1 -NoRun    build + install only
param(
    [switch]$NoRun
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $Root

function Log($msg) { Write-Host "`n==> $msg" }

Log 'killing running denia processes (exe is locked while running)'
Stop-Process -Name denia -Force -ErrorAction SilentlyContinue
# 改名前的旧进程:还跑着就连旧 exe 一起杀。
Stop-Process -Name dsh-rs -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 1

Log 'building web console'
$web = Join-Path $Root 'web'
if (-not (Test-Path (Join-Path $web 'node_modules'))) {
    Push-Location $web
    pnpm install
    Pop-Location
}
Push-Location $web
pnpm build
Pop-Location

Log 'release build + global install'
cargo install --path crates/server --force

# 改名前的旧二进制:安装成功后清掉,不留残留。
Remove-Item (Join-Path $env:USERPROFILE '.cargo\bin\dsh-rs.exe') -Force -ErrorAction SilentlyContinue

if (-not $NoRun) {
    Log 'starting denia in background'
    Start-Process -FilePath 'denia' -WindowStyle Hidden
    Start-Sleep -Seconds 2
    try {
        $status = (Invoke-WebRequest -Uri 'http://127.0.0.1:3600/' -UseBasicParsing -TimeoutSec 5).StatusCode
        Log "server ready (HTTP $status) — http://127.0.0.1:3600/"
    } catch {
        Write-Warning "denia started but probe failed: $($_.Exception.Message)"
    }
} else {
    Log 'done (server not started; pass nothing to auto-start)'
    Write-Host 'start with: denia        (console on http://127.0.0.1:3600)'
}
