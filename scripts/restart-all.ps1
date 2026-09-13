# 一次性重启脚本(独立分离进程执行):正式版 3600 + 开发版 3601 都重建重启。
# 杀正式 denia.exe 会中断它承载的对话,故由本脚本脱离会话自行完成全部步骤。
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $Root
$log = Join-Path $Root 'restart-all.log'
Start-Transcript -Path $log -Force | Out-Null

function Log($msg) { Write-Host "`n==> $msg" }

try {
    Log 'kill running instances'
    Stop-Process -Name denia -Force -ErrorAction SilentlyContinue
    Stop-Process -Name dsh-rs -Force -ErrorAction SilentlyContinue
    Stop-Process -Name denia_develop -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 1

    Log 'global install (cargo install, incremental)'
    cargo install --path crates/server --force
    if ($LASTEXITCODE -ne 0) { throw 'cargo install failed' }
    Remove-Item (Join-Path $env:USERPROFILE '.cargo\bin\dsh-rs.exe') -Force -ErrorAction SilentlyContinue

    Log 'start formal instance (3600)'
    Start-Process -FilePath (Join-Path $env:USERPROFILE '.cargo\bin\denia.exe') -WindowStyle Hidden

    Log 'start dev instance (3601, shared ~/.denia, web/dist from disk)'
    Copy-Item -Force 'target\release\denia.exe' 'denia_develop.exe'
    Start-Process -FilePath (Join-Path $Root 'denia_develop.exe') `
        -ArgumentList '--port', '3601', '--home', (Join-Path $env:USERPROFILE '.denia'), '--web', (Join-Path $Root 'web\dist') `
        -WindowStyle Hidden

    Start-Sleep -Seconds 3
    foreach ($port in 3600, 3601) {
        try {
            $status = (Invoke-WebRequest -Uri "http://127.0.0.1:$port/" -UseBasicParsing -TimeoutSec 5).StatusCode
            Log "probe :$port -> HTTP $status"
        } catch {
            Log "probe :$port failed: $($_.Exception.Message)"
        }
    }
    Log 'done'
} finally {
    Stop-Transcript | Out-Null
}
