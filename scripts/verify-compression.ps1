# 验证远程/隧道性能改动的实际响应。
#
# 覆盖四件事:
#   1. 静态资源按 Accept-Encoding 选构建期预压缩实体(br > gzip > 明文);
#   2. API JSON 走 gzip;
#   3. SSE 与 WebSocket 升级响应**不得**被压缩(否则流式输出与终端功能会坏);
#   4. 安全边界(路径穿越、鉴权)没被这轮改动碰坏。
#
# 用 curl 而不是 .NET HttpClient:后者把 header 分成 request/response/content
# 三类,取 content-encoding 要走 content.Headers,脚本里容易写错;curl 直接看线上原文。
#
#   pwsh scripts/verify-compression.ps1
#   BASE=http://127.0.0.1:3600 pwsh scripts/verify-compression.ps1
$ErrorActionPreference = 'Continue'
$Base = if ($env:BASE) { $env:BASE } else { 'http://127.0.0.1:3601' }
$Out = Join-Path $env:TEMP 'denia-verify-body.bin'
$Head = Join-Path $env:TEMP 'denia-verify-head.txt'
$script:fail = 0
$script:skip = 0

# 断言必须把"判定失败"与"判定根本没跑起来"分开。之前用 [bool] 强转参数,
# 传入数组时 PowerShell 在**参数绑定阶段**就抛错、Assert 函数体压根没执行,
# 结果 fail 不累加、脚本收尾还打印"全部断言通过" —— 那是谎报。
# 现在统一收 [object] 并在函数内自行判定,任何异常形态一律记 FAIL。
function Assert([object]$cond, [string]$what) {
  $scalar = $cond
  if ($cond -is [array]) {
    # 数组形式的 `-match` 结果是"命中的元素",长度>0 即命中。
    $scalar = (@($cond).Count -gt 0)
  }
  $ok = $false
  try { $ok = [bool]$scalar } catch { $ok = $false }
  if ($ok) { Write-Host "  ok    $what" -ForegroundColor Green }
  else { Write-Host "  FAIL  $what" -ForegroundColor Red; $script:fail++ }
}
function AssertExactly($actual, $expected, [string]$what) {
  # 显式比较两条标量,避免把数组比较的结果当布尔用。
  Assert (@($actual).Count -eq 1 -and "$($actual)" -eq "$expected") "$what (实际 '$($actual -join '|')',期望 '$expected')"
}
function Skip([string]$what) {
  Write-Host "  SKIP  $what" -ForegroundColor DarkGray
  $script:skip++
}

function Probe([string]$label, [string]$path, [string]$acceptEncoding, [string[]]$extra = @()) {
  $curlArgs = @('-s', '-D', $Head, '-o', $Out, '--max-time', '15') + $extra
  if ($acceptEncoding) { $curlArgs += @('-H', "Accept-Encoding: $acceptEncoding") }
  & curl.exe @curlArgs "$Base$path" 2>$null | Out-Null
  $head = @(Get-Content $Head -ErrorAction SilentlyContinue)
  $bytes = if (Test-Path $Out) { (Get-Item $Out).Length } else { 0 }
  # 缺头时统一成 '':空管道会收成 Object[],后续与 '' 比较就成了数组比较。
  $value = {
    param([string]$name)
    $line = @($head | Where-Object { $_ -match "^(?i)$name\s*:" } | Select-Object -First 1)
    if ($line.Count -eq 0) { '' } else { ($line[0] -replace "^(?i)$name\s*:", '').Trim() }
  }
  $statusLine = @($head | Where-Object { $_ -match '^HTTP/' } | Select-Object -Last 1)
  [pscustomobject]@{
    Label = $label
    Status = if ($statusLine.Count) { $statusLine[0] -replace '^HTTP/1\.[012]\s*', '' } else { '' }
    Encoding = (& $value 'content-encoding')
    ContentType = (& $value 'content-type')
    CacheControl = (& $value 'cache-control')
    Vary = (& $value 'vary')
    Bytes = $bytes
    HeadLines = $head
  }
}

Write-Host "目标: $Base" -ForegroundColor Cyan
$results = [System.Collections.Generic.List[object]]::new()
$jsFile = Get-ChildItem web\dist\assets -Filter 'index-*.js' | Select-Object -First 1
if (-not $jsFile) { throw '找不到主包 JS 产物(先跑 cd web; pnpm build)' }
$plain = $jsFile.Length

$results += Probe 'js  br' "/assets/$($jsFile.Name)" 'gzip, deflate, br'
$results += Probe 'js  gzip-only' "/assets/$($jsFile.Name)" 'gzip'
$results += Probe 'js  无 AE' "/assets/$($jsFile.Name)" $null
$results += Probe 'index.html' '/' 'br'
$results += Probe 'api status  gzip' '/api/remote/status' 'gzip'
$results += Probe 'api status  无 AE' '/api/remote/status' $null
$results += Probe 'api settings gzip' '/api/settings' 'gzip'
$results += Probe 'q=0 协商掉' '/api/remote/status' 'gzip;q=0, br;q=0'

$results | Format-Table -AutoSize Label, Status, Encoding, Bytes, ContentType | Out-String -Width 200
'主包明文 = {0:N0} B;隧道上实际要传 {1:N0} B(br),省 {2:P0}。' -f $plain, $results[0].Bytes, (1 - $results[0].Bytes / $plain)

Write-Host ''
Write-Host '=== 断言:静态资源与 API ===' -ForegroundColor Cyan
$jsBr = $results[0]
$jsGz = $results[1]
$jsNone = $results[2]
AssertExactly $jsBr.Encoding 'br' '支持 br 的客户端拿到 brotli 预压缩实体'
Assert ($jsBr.Bytes -gt 0 -and $jsBr.Bytes -lt ($plain / 3)) "br 体积低于明文 1/3($($jsBr.Bytes) B vs $($plain) B)"
AssertExactly $jsGz.Encoding 'gzip' '只支持 gzip 的客户端拿到 gzip'
Assert ($jsGz.Bytes -gt 0 -and $jsGz.Bytes -lt $plain) "gzip 确实更小($($jsGz.Bytes) B)"
AssertExactly $jsNone.Encoding '' '不支持压缩的客户端拿到明文、无 Content-Encoding'
Assert ($jsNone.Bytes -eq $plain) "明文分支字节数与磁盘一致($($jsNone.Bytes))"
Assert ($jsBr.Vary -match 'Accept-Encoding') '带 Vary: Accept-Encoding(防中间缓存把 gzip 发给支持 br 的客户端)'
Assert ($jsBr.ContentType -match 'javascript') "content-type 没被 .br 后缀带偏('$($jsBr.ContentType)')"
Assert ($results[3].CacheControl -match 'no-store') 'index.html 不缓存'
AssertExactly $results[4].Encoding 'gzip' 'API JSON 走 gzip'
AssertExactly $results[5].Encoding '' 'API 对不支持压缩的客户端发明文'
AssertExactly $results[6].Encoding 'gzip' '大 JSON(/api/settings)也走 gzip'
Assert ($results[7].Status -match '^200') "q=0 协商掉压缩后仍 200 而非 500:$($results[7].Status)"

Write-Host ''
Write-Host '=== 断言:SSE 不得被压缩 ===' -ForegroundColor Cyan
$sse = Probe 'sse' '/api/events' 'gzip, br' @('-N', '--max-time', '3')
AssertExactly $sse.Encoding '' 'SSE 响应未被压缩(压缩缓冲会把流式帧攒成一批)'
Assert ($sse.ContentType -match 'text/event-stream') "SSE content-type 正确('$($sse.ContentType)')"

Write-Host ''
Write-Host '=== 断言:WebSocket 升级仍是 101 且不被压缩 ===' -ForegroundColor Cyan
# WS 端点是 /api/terminals/{id}/ws,要先开一个真实终端才能握手。
# 这一条值端到端跑:谓词错了的症状是"终端面板打不开",从 HTTP 层看不出来。
$terminalId = $null
try {
  $created = & curl.exe -s -X POST --max-time 10 -H 'content-type: application/json' `
    -d '{}' "$Base/api/terminals" 2>$null | Out-String
  # 返回形状是 {"terminal":{…}};写错字段会让整个 WS 探测静默跳过。
  $terminalId = ($created | ConvertFrom-Json).terminal.id
  if (-not $terminalId) { throw "响应里没有 terminal.id:$created" }
} catch {
  Skip "创建终端失败,跳过 WS 探测:$($_.Exception.Message)"
}
if ($terminalId) {
  try {
    $wsHeadFile = Join-Path $env:TEMP 'denia-ws-head.txt'
    & curl.exe -s -D $wsHeadFile -o NUL --max-time 5 `
      -H 'Connection: Upgrade' -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' `
      -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' -H 'Accept-Encoding: gzip' `
      "$Base/api/terminals/$terminalId/ws" 2>$null | Out-Null
    $wsLines = @(Get-Content $wsHeadFile -ErrorAction SilentlyContinue)
    $wsStatus = @($wsLines | Where-Object { $_ -match '^HTTP/' } | Select-Object -Last 1)
    Assert (@($wsStatus).Count -gt 0 -and "$wsStatus" -match ' 101 ') "升级响应是 101:$($wsStatus -join '')"
    Assert (@($wsLines | Where-Object { $_ -match '^(?i)content-encoding' }).Count -eq 0) '升级响应未被塞 Content-Encoding'
  } finally {
    & curl.exe -s -X DELETE --max-time 10 "$Base/api/terminals/$terminalId" 2>$null | Out-Null
    Remove-Item $wsHeadFile -Force -ErrorAction SilentlyContinue
  }
}

Write-Host ''
Write-Host '=== 断言:安全边界未被碰坏 ===' -ForegroundColor Cyan
# --path-as-is:否则 curl 会先把 ../ 折叠掉,测的就不是服务端了。
$trav = Probe 'traversal' '/../remote/audit.jsonl' 'br' @('--path-as-is')
Assert ($trav.Status -notmatch '^200' -or $trav.Bytes -lt 16) "越界路径不返回内容:$($trav.Status) / $($trav.Bytes) B"
$trav2 = Probe 'traversal2' '/assets/../../../../Windows/win.ini' 'br' @('--path-as-is')
Assert ($trav2.Status -notmatch '^200') "编码外的深层穿越同样被挡:$($trav2.Status)"
$api = Probe 'api 可达' '/api/remote/status' 'br'
Assert ($api.Status -match '^(200|401|403)') "接口鉴权行为未变:$($api.Status)"

Remove-Item $Out, $Head -Force -ErrorAction SilentlyContinue
Write-Host ''
if ($script:fail -gt 0) {
  Write-Host "$($script:fail) 条断言失败(另有 $($script:skip) 条跳过)" -ForegroundColor Red
  exit 1
}
if ($script:skip -gt 0) {
  Write-Host "断言全部通过,但有 $($script:skip) 条被跳过 —— 去看跳过了什么" -ForegroundColor Yellow
  exit 0
}
Write-Host '全部断言通过' -ForegroundColor Green
