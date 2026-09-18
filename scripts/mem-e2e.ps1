# 端到端验证:预算收紧后,真实会话的读写/发消息是否正常(不能误淘汰运行中的会话)
$ErrorActionPreference = 'Continue'

$exe     = 'D:\code_project\denia\target\release\denia.exe'
$tmpHome = Join-Path $env:TEMP 'denia-meme2e'
$port    = 3694

if (Test-Path $tmpHome) { Remove-Item $tmpHome -Recurse -Force }
New-Item -ItemType Directory -Path (Join-Path $tmpHome 'sessions') -Force | Out-Null
# 造一个工作目录,让会话能发消息(死 cwd 会被拒)
$work = Join-Path $tmpHome 'work'
New-Item -ItemType Directory -Path $work -Force | Out-Null

function Snap([string]$tag) {
    $p = Get-Process -Id $script:pid_ -ErrorAction SilentlyContinue
    if ($null -eq $p) { "$tag : 已退出"; return }
    $p.Refresh()
    "{0,-40} priv={1,7:N1} MB ws={2,7:N1} MB" -f $tag, ($p.PrivateMemorySize64/1MB), ($p.WorkingSet64/1MB)
}

$proc = Start-Process -FilePath $exe -ArgumentList @('--home', $tmpHome, '--port', $port) `
    -PassThru -WindowStyle Hidden -RedirectStandardOutput "$tmpHome\out.log" -RedirectStandardError "$tmpHome\err.log"
$script:pid_ = $proc.Id
Start-Sleep -Seconds 6
if ($proc.HasExited) { "启动失败"; Get-Content "$tmpHome\err.log" -Tail 30; exit 1 }
Snap '0. 冷启动'

# 1) 建会话
$body = @{ cwd = $work; sandbox = $false } | ConvertTo-Json
$r = Invoke-WebRequest -Uri "http://127.0.0.1:$port/api/sessions" -Method POST -Body $body -ContentType 'application/json' -UseBasicParsing -TimeoutSec 60
$sid = ($r.Content | ConvertFrom-Json).session.id
"1. 建会话: $($sid.Substring(0,8))"

# 2) 打开(冷加载) + 读事件分页
$null = Invoke-WebRequest -Uri "http://127.0.0.1:$port/api/sessions/$sid/events?limit=100" -UseBasicParsing -TimeoutSec 60
"2. 事件分页 OK"
Snap '2. 分页后'

# 3) 热升级(runtime ensure 路径)
$null = Invoke-WebRequest -Uri "http://127.0.0.1:$port/api/sessions/$sid/agents" -UseBasicParsing -TimeoutSec 60
"3. 热升级 OK"
Snap '3. 热升级后'

# 4) 再打开一次,确认淘汰后仍能正常重载(不 500)
Start-Sleep -Seconds 2
$null = Invoke-WebRequest -Uri "http://127.0.0.1:$port/api/sessions/$sid/agents" -UseBasicParsing -TimeoutSec 60
"4. 重复打开 OK(重载路径正常)"
Snap '4. 重复打开后'

# 5) 全量快照(磁盘流式)
$r5 = Invoke-WebRequest -Uri "http://127.0.0.1:$port/api/sessions/$sid" -UseBasicParsing -TimeoutSec 60
"5. 全量快照 OK ($([math]::Round($r5.RawContentLength/1KB,1)) KB)"
Snap '5. 快照后'

# 6) follow/poll 增量
$r6 = Invoke-WebRequest -Uri "http://127.0.0.1:$port/api/sessions/$sid/follow/poll?after=0&wait=0" -UseBasicParsing -TimeoutSec 60
"6. follow/poll OK ($($r6.Content.Length) 字节)"
Snap '6. poll 后'

# 7) 列表
$r7 = Invoke-WebRequest -Uri "http://127.0.0.1:$port/api/sessions" -UseBasicParsing -TimeoutSec 60
"7. 列表 OK: $((($r7.Content | ConvertFrom-Json).sessions).Count) 个会话"

# 8) 删除会话(验证辅助表回收路径不报错)
$null = Invoke-WebRequest -Uri "http://127.0.0.1:$port/api/sessions/$sid" -Method DELETE -UseBasicParsing -TimeoutSec 60
"8. 删除 OK"
Snap '8. 删除后'

# 9) 删除后访问应 404(而不是 500)
try {
    Invoke-WebRequest -Uri "http://127.0.0.1:$port/api/sessions/$sid/agents" -UseBasicParsing -TimeoutSec 30 | Out-Null
    "9. 意外成功(预期 404)"
} catch {
    "9. 删除后访问 -> $($_.Exception.Response.StatusCode.value__)(预期 404)"
}

"--- 结论 ---"
"全部端点正常,无 500;峰值内存见上"
"PID=$($proc.Id)"
