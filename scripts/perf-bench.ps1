# 可重复性能基线/对比脚本。
# 用法:
#   pwsh scripts/perf-bench.ps1 -Home C:\tmp\denia-perf -LightCount 10000 -LongEvents 100000 -LongTargetMB 50 -Port 3699 -Exe .\denia_develop.exe [-Legacy]
#
# -Legacy: 旧版只有全量 getSession,用 full endpoint 测长会话;新版默认测 events page。
param(
  [Parameter(Mandatory=$true)][string]$Home,
  [Parameter(Mandatory=$true)][int]$LightCount,
  [Parameter(Mandatory=$true)][int]$LongEvents,
  [int]$LongTargetMB = 0,
  [int]$Port = 3699,
  [string]$Exe = '.\denia_develop.exe',
  [switch]$Legacy
)

$ErrorActionPreference = 'Stop'
$LongId = 'aaaaaaaa-0000-4000-8000-000000000001'

Write-Host "==> gen fixtures (light=$LightCount long=$LongEvents)"
node (Join-Path $PSScriptRoot 'perf-gen.cjs') $Home $LightCount $LongEvents $LongTargetMB

Write-Host "==> starting server on $Port"
$exe = (Resolve-Path $Exe).Path
$args = @('--port', "$Port", '--home', $Home, '--web', (Resolve-Path (Join-Path $PSScriptRoot '..\web\dist')).Path)
$proc = Start-Process -FilePath $exe -ArgumentList $args -WindowStyle Hidden -PassThru
try {
  $ready = $false
  for ($i = 0; $i -lt 60; $i++) {
    Start-Sleep -Milliseconds 500
    try {
      $null = Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$Port/api/settings" -TimeoutSec 2
      $ready = $true
      break
    } catch { }
  }
  if (-not $ready) { throw "server did not become ready on $Port" }

  function Measure-Api([string]$Uri) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $r = Invoke-WebRequest -UseBasicParsing $Uri -TimeoutSec 300
    $sw.Stop()
    return [pscustomobject]@{
      Uri = $Uri
      ms = $sw.ElapsedMilliseconds
      bytes = $r.Content.Length
    }
  }

  $results = @()

  $list = Measure-Api "http://127.0.0.1:$Port/api/sessions"
  $results += $list

  if ($Legacy) {
    $long = Measure-Api "http://127.0.0.1:$Port/api/sessions/$LongId"
    $results += $long
  } else {
    $long = Measure-Api "http://127.0.0.1:$Port/api/sessions/$LongId/events?limit=500"
    $results += $long
    # 2 次翻页(取第 500 条之前的 500 条)
    $oldest = 100000 - 500
    $older = Measure-Api "http://127.0.0.1:$Port/api/sessions/$LongId/events?before=$oldest&limit=500"
    $results += $older
  }

  $proc.Refresh()
  $rss = [math]::Round($proc.WorkingSet64 / 1MB, 1)
  $pm = [math]::Round($proc.PrivateMemorySize64 / 1MB, 1)
  Write-Host "==> results"
  $results | Format-Table -AutoSize
  Write-Host "backend rss MB=$rss pm MB=$pm"
  Write-Host "backend extra rss vs baseline? use psutil processes baseline in caller"
} finally {
  if ($proc -and -not $proc.HasExited) {
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
  }
}
