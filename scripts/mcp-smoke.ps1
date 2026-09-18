# MCP 端到端冒烟:加服务器 → 工具出现 → 关工具 → 删服务器。
# 用 fixture MCP 服务器(denia-mcp-fixture)跑真实子进程。
$ErrorActionPreference = 'Stop'
$base = 'http://127.0.0.1:3601'
$fixture = 'D:\code_project\denia\target\debug\denia-mcp-fixture.exe'

if (-not (Test-Path $fixture)) {
  cargo build -p denia-mcp --bin denia-mcp-fixture
  if ($LASTEXITCODE -ne 0) { throw 'fixture 构建失败' }
}

function Show($label, $value) { Write-Host "`n==> $label"; Write-Host $value }

function Put($path, $body) {
  Invoke-RestMethod -Uri "$base$path" -Method Put -ContentType 'application/json' -Body ($body | ConvertTo-Json -Depth 8)
}

function PostJson($path, $body) {
  Invoke-RestMethod -Uri "$base$path" -Method Post -ContentType 'application/json' -Body ($body | ConvertTo-Json -Depth 8)
}

# 1) 初始快照
Show '初始 /api/mcp' ((Invoke-WebRequest "$base/api/mcp" -UseBasicParsing).Content)

# 2) 添加服务器
$server = @{
  id            = 'fx'
  transport     = 'stdio'
  command       = $fixture
  args          = @()
  env           = @{}
  cwd           = $null
  enabled       = $true
  disabledTools = @()
}
$after = Put '/api/mcp/servers' @{ server = $server }
Show '添加后' (($after | ConvertTo-Json -Depth 8))
if ($after.servers[0].status -ne 'connected') { throw "期望 connected,实际 $($after.servers[0].status)" }
if ($after.toolCount -ne 2) { throw "期望 2 个工具,实际 $($after.toolCount)" }

# 3) 关掉 echo 工具
$off = PostJson '/api/mcp/tools' @{ server = 'fx'; tool = 'echo'; enabled = $false }
Show '关闭 echo 后' (($off | ConvertTo-Json -Depth 8))
if ($off.toolCount -ne 1) { throw "关闭后应剩 1 个工具,实际 $($off.toolCount)" }

# 4) 禁用服务器
$disabled = PostJson '/api/mcp/servers/fx' @{ action = 'disable' }
Show '禁用服务器后' (($disabled | ConvertTo-Json -Depth 8))
if ($disabled.toolCount -ne 0) { throw "禁用后应为 0 个工具,实际 $($disabled.toolCount)" }

# 5) 重新启用
$enabled = PostJson '/api/mcp/servers/fx' @{ action = 'enable' }
if ($enabled.servers[0].status -ne 'connected') { throw "重新启用后应 connected" }
Show '重新启用后' (($enabled | ConvertTo-Json -Depth 8))

# 6) 删除服务器
Invoke-RestMethod -Uri "$base/api/mcp/servers/fx" -Method Delete | Out-Null
$final = Invoke-WebRequest "$base/api/mcp" -UseBasicParsing
Show '删除后' $final.Content

# 7) 非法配置必须被拒(fail loud)
try {
  Put '/api/mcp/servers' @{ server = @{ id = 'Bad Id'; transport = 'stdio'; command = 'x'; args = @(); env = @{}; disabledTools = @(); enabled = $true } } | Out-Null
  throw '非法 id 竟然被接受了'
} catch {
  Write-Host "`n==> 非法 id 被拒绝(符合预期): $($_.Exception.Message)"
}

Write-Host "`nMCP 冒烟全部通过"
