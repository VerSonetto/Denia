# 验证:驻留预算收紧后,连续热加载多个大会话是否被约束在预算内
$ErrorActionPreference = 'Continue'

$exe     = 'D:\code_project\denia\target\release\denia.exe'
$tmpHome = Join-Path $env:TEMP 'denia-memverify'
$port    = 3695

if (Test-Path $tmpHome) { Remove-Item $tmpHome -Recurse -Force }
New-Item -ItemType Directory -Path (Join-Path $tmpHome 'sessions') -Force | Out-Null

# 复制 10 个最大的会话(超过新的 8 个上限,用于触发淘汰)
$srcRoot = Join-Path $env:USERPROFILE '.denia\sessions'
$top = Get-ChildItem $srcRoot -Directory | ForEach-Object {
    $f = Join-Path $_.FullName 'session.jsonl'
    if (Test-Path $f) { [PSCustomObject]@{ Id = $_.Name; MB = (Get-Item $f).Length } }
} | Sort-Object MB -Descending | Select-Object -First 10

foreach ($s in $top) {
    $d = Join-Path $tmpHome "sessions\$($s.Id)"
    New-Item -ItemType Directory -Path $d -Force | Out-Null
    Copy-Item (Join-Path $srcRoot "$($s.Id)\session.jsonl") -Destination (Join-Path $d 'session.jsonl')
}
"复制 $($top.Count) 个会话, 合计 $([math]::Round(($top | Measure-Object MB -Sum).Sum/1MB,1)) MB"

function Snap([string]$tag) {
    $p = Get-Process -Id $script:pid_ -ErrorAction SilentlyContinue
    if ($null -eq $p) { "$tag : 已退出"; return }
    $p.Refresh()
    "{0,-44} priv={1,7:N1} MB  ws={2,7:N1} MB  peak={3,7:N1} MB" -f $tag, ($p.PrivateMemorySize64/1MB), ($p.WorkingSet64/1MB), ($p.PeakPagedMemorySize64/1MB)
}

$proc = Start-Process -FilePath $exe -ArgumentList @('--home', $tmpHome, '--port', $port) `
    -PassThru -WindowStyle Hidden -RedirectStandardOutput "$tmpHome\out.log" -RedirectStandardError "$tmpHome\err.log"
$script:pid_ = $proc.Id
Start-Sleep -Seconds 6
if ($proc.HasExited) { "启动失败"; Get-Content "$tmpHome\err.log" -Tail 30; exit 1 }
Snap '0. 冷启动'

$i = 0
$max = 0.0
foreach ($s in $top) {
    $i++
    try {
        Invoke-WebRequest -Uri "http://127.0.0.1:$port/api/sessions/$($s.Id)/agents" -UseBasicParsing -TimeoutSec 300 | Out-Null
    } catch { "  第 $i 个失败: $($_.Exception.Message)" }
    Start-Sleep -Milliseconds 1500
    $p = Get-Process -Id $script:pid_
    $p.Refresh()
    $mb = $p.PrivateMemorySize64/1MB
    if ($mb -gt $max) { $max = $mb }
    Snap ("{0}. 热加载 {1}({2} MB)" -f $i, $s.Id.Substring(0,8), [math]::Round($s.MB/1MB,1))
}

"--- 关键结论 ---"
"10 个大会话(合计 $([math]::Round(($top | Measure-Object MB -Sum).Sum/1MB,1)) MB 日志)连续热加载后,"
"denia.exe 峰值私有内存 = $([math]::Round($max,1)) MB"
"预算: 8 会话 / 64 MB 驻留字节 -> 预期实际内存约 100~160 MB 以内"
"PID=$($proc.Id)"
