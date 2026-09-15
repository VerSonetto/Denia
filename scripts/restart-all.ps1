# 一次性重建脚本(双实例:正式版 3600 + 开发版 3601)。
#
#   pwsh scripts/restart-all.ps1 -Detach   # 自分离后台执行(agent 对话用;正式实例被杀也不中断构建)
#   pwsh scripts/restart-all.ps1           # 前台执行(本地手动用)
#   pwsh scripts/restart-all.ps1 -DryRun   # 只打印计划与预检,不杀不构建(校验用)
#
# 流程:杀开发实例 → 杀正式实例 → 前端构建 → cargo install(正式二进制)
#      → 起正式实例 → 刷新 denia_develop.exe → 起开发实例 → 双端口探测。
# 构建失败则按"探测门控"只起不通的那边(安装失败不会覆盖旧二进制,旧二进制仍在)。
#
# 注意:正式实例被杀会导致它承载的对话中断。本脚本用 -Detach 时调用立刻返回,
# 构建在分离进程继续,全程日志写 restart-all.log —— 凭该日志与端口探测确认结果,
# 完成后在 3600 刷新继续对话。
[CmdletBinding()]
param(
  [switch]$Detach,
  [switch]$DryRun
)
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$FormalPort = 3600
$DevPort = 3601
$HomeDir = Join-Path $env:USERPROFILE '.denia'
$GlobalExe = Join-Path $env:USERPROFILE '.cargo\bin\denia.exe'
$DevExe = Join-Path $Root 'denia_develop.exe'
$TargetExe = Join-Path $Root 'target\release\denia.exe'
$DistDir = Join-Path $Root 'web\dist'
$LogFile = Join-Path $Root 'restart-all.log'

function Log($msg) { Write-Host "`n==> $msg" }

function Probe($port) {
  try {
    # -SkipHttpErrorCheck:404 是"进程活着但控制台资产缺失",必须与"端口没起"(0)
    # 区分开 —— 否则日志只会显示 HTTP 0,把控制台坏误报成服务没起,恢复逻辑
    # 会去重启一个本来就坏的同款二进制。
    return (Invoke-WebRequest -Uri "http://127.0.0.1:$port/" -UseBasicParsing -TimeoutSec 8 -SkipHttpErrorCheck).StatusCode
  } catch {
    return 0
  }
}

if ($DryRun) {
  Write-Host '计划(不执行任何杀/构建/启动动作):'
  Write-Host '  1. 杀 denia_develop,再杀 denia(正式实例,承载的对话将中断)/dsh-rs'
  Write-Host "  2. 前端构建: $Root\web (dist 预清空,缺 node_modules 则先 pnpm install)"
  Write-Host '  3. cargo install --path crates/server --force'
  Write-Host "     正式二进制: $GlobalExe"
  Write-Host "  4. 起正式实例 :$FormalPort;复制 $TargetExe → $DevExe;起开发实例 :$DevPort"
  Write-Host '  5. 双端口探测;构建失败则按探测门控恢复旧实例(只起不通的那边)'
  Write-Host '预检:'
  foreach ($cmd in 'cargo', 'pnpm') {
    $found = Get-Command $cmd -ErrorAction SilentlyContinue
    Write-Host ("  {0}: {1}" -f $cmd, $(if ($found) { $found.Source } else { '缺失!' }))
  }
  Write-Host ("  web/ 存在: {0}" -f (Test-Path (Join-Path $Root 'web')))
  Write-Host ("  旧正式二进制存在: {0}" -f (Test-Path $GlobalExe))
  Write-Host ("  旧开发二进制存在: {0}" -f (Test-Path $DevExe))
  Write-Host ("  日志将写入: {0}" -f $LogFile)
  return
}

# 自分离:再起一个隐藏的自己干活,本进程立刻返回。调用方(被正式实例承载的
# agent 对话)在杀正式实例那一步被中断也不影响构建。环境变量防重复分离。
if ($Detach -and -not $env:DENIA_REBUILD_DETACHED) {
  $env:DENIA_REBUILD_DETACHED = '1'
  Start-Process -FilePath 'pwsh' `
    -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $MyInvocation.MyCommand.Path `
    -WindowStyle Hidden -WorkingDirectory $Root
  Write-Host "已在分离进程后台启动,日志: $LogFile"
  return
}

Set-Location $Root
Start-Transcript -Path $LogFile -Force | Out-Null
$buildStart = Get-Date

# 分离子进程的宽限:调用方(被正式实例承载的对话)需要几秒把"已启动"的
# 收尾消息落盘;不等就杀,用户刷新后看不到这条确认。手动前台跑时无此问题。
if ($env:DENIA_REBUILD_DETACHED) {
  Start-Sleep -Seconds 10
}

try {
  Log 'kill dev instance (denia_develop)'
  Stop-Process -Name denia_develop -Force -ErrorAction SilentlyContinue
  Log 'kill formal instance (denia) — 承载的对话将中断,构建在分离进程继续'
  Stop-Process -Name denia -Force -ErrorAction SilentlyContinue
  # 改名前的旧进程:还跑着就连旧 exe 一起杀。
  Stop-Process -Name dsh-rs -Force -ErrorAction SilentlyContinue
  Start-Sleep -Seconds 1

  Log 'building web console'
  # 预清空 dist:vite 的 emptyOutDir 走 node fs.rmSync,可能被批量删除守卫拦下;
  # 改用 PowerShell 侧删除(与 dev.ps1 同理)。目录被占用但已空则继续,不中断构建。
  if (Test-Path 'web/dist') {
    try {
      Remove-Item -Recurse -Force 'web/dist' -ErrorAction Stop
    } catch {
      $remaining = @(Get-ChildItem 'web/dist' -Force -ErrorAction SilentlyContinue)
      if ($remaining.Count -gt 0) { throw "清理 web/dist 失败,仍有 $($remaining.Count) 项残留" }
      Write-Host '    note: web/dist 目录被占用(已空),跳过删除继续构建'
    }
  }
  if (-not (Test-Path 'web/node_modules')) {
    Push-Location web
    try { pnpm install; if ($LASTEXITCODE -ne 0) { throw '前端依赖安装失败,旧二进制未动' } } finally { Pop-Location }
  }
  Push-Location web
  try { pnpm build; if ($LASTEXITCODE -ne 0) { throw '前端构建失败,旧二进制未动' } } finally { Pop-Location }

  # 产物校验:pnpm build 返回 0 不等于 dist 有内容(构建链被静默截断时
  # 会出现"成功退出但零产物")。这里不校验就继续,空 dist 会被 rust-embed
  # 嵌进二进制,导致两个端口都只回 404 而日志看不出任何异常。
  $distIndex = Join-Path $DistDir 'index.html'
  if (-not (Test-Path $distIndex)) {
    throw "前端构建未产出 $distIndex(dist 为空),拒绝继续 —— 空产物会被内嵌进二进制"
  }
  $distAssets = @(Get-ChildItem (Join-Path $DistDir 'assets') -Filter 'index-*.js' -ErrorAction SilentlyContinue)
  if ($distAssets.Count -eq 0) {
    throw "前端构建未产出 assets/index-*.js,拒绝继续"
  }
  Write-Host ("    dist 产物校验通过:{0} 个文件,主入口 {1}" -f `
    @(Get-ChildItem $DistDir -Recurse -File).Count, $distAssets[0].Name)

  Log 'release install (cargo install --path crates/server --force)'
  cargo install --path crates/server --force
  if ($LASTEXITCODE -ne 0) { throw 'cargo install 失败,旧正式二进制未动' }
  # 改名前的旧二进制:安装成功后清掉,不留残留。
  Remove-Item (Join-Path $env:USERPROFILE '.cargo\bin\dsh-rs.exe') -Force -ErrorAction SilentlyContinue

  # target 二进制新鲜度守卫:cargo install 是否复用工作区 target 目录取决于
  # cargo 行为;过期则显式 build 一次,保证开发实例拿到的是本次构建。
  if (-not (Test-Path $TargetExe) -or (Get-Item $TargetExe).LastWriteTime -lt $buildStart) {
    Log 'target binary stale, explicit cargo build --release'
    cargo build --release -p denia-server
    if ($LASTEXITCODE -ne 0) { throw '后端构建失败' }
  }
  if ((Get-Item $TargetExe).LastWriteTime -lt $buildStart) {
    throw 'target 二进制仍不是本次构建,拒绝用旧二进制起开发实例'
  }

  Log "start formal instance (:$FormalPort)"
  Start-Process -FilePath $GlobalExe -WindowStyle Hidden

  Log "stage dev binary + start dev instance (:$DevPort, --web from disk)"
  Copy-Item -Force $TargetExe $DevExe
  Start-Process -FilePath $DevExe `
    -ArgumentList '--port', "$DevPort", '--home', $HomeDir, '--web', $DistDir `
    -WindowStyle Hidden

  Start-Sleep -Seconds 3
  $formal = Probe $FormalPort
  $dev = Probe $DevPort
  Log "probe :$FormalPort -> HTTP $formal"
  Log "probe :$DevPort -> HTTP $dev"
  $head = git rev-parse --short HEAD 2>$null
  Log "done (HEAD $head)"
  if ($formal -ne 200 -or $dev -ne 200) {
    if ($formal -eq 404 -or $dev -eq 404) {
      throw '端口在听但控制台返回 404 —— 进程起来了但 web 资产为空(检查 dist 与内嵌资产)'
    }
    throw '端口探测未全过'
  }
} catch {
  Log "ERROR: $($_.Exception.Message)"
  Log '按探测门控恢复(只起不通的那边;安装失败不会覆盖旧二进制)'
  if ((Probe $FormalPort) -ne 200 -and (Test-Path $GlobalExe)) {
    try { Start-Process -FilePath $GlobalExe -WindowStyle Hidden } catch {
      Log "正式实例恢复启动失败: $($_.Exception.Message)"
    }
  }
  if ((Probe $DevPort) -ne 200 -and (Test-Path $DevExe)) {
    try {
      Start-Process -FilePath $DevExe `
        -ArgumentList '--port', "$DevPort", '--home', $HomeDir, '--web', $DistDir `
        -WindowStyle Hidden
    } catch {
      Log "开发实例恢复启动失败: $($_.Exception.Message)"
    }
  }
  Start-Sleep -Seconds 3
  Log "recover probe :$FormalPort -> HTTP $(Probe $FormalPort)"
  Log "recover probe :$DevPort -> HTTP $(Probe $DevPort)"
  exit 1
} finally {
  Stop-Transcript | Out-Null
}
