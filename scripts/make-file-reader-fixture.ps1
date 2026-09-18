# 生成一个用于验收「文件读取标签页」四类触发点的测试会话。
#
# 直接写 session.jsonl(与后端同一格式),这样四类触发点所需的素材一次到位:
#   1. Markdown 文件引用链接(工作区内相对路径 + 一个外部 URL 作对照)
#   2. edit / write_file 工具调用的路径
#   3. 轮次产物列表(由 write_file/edit 结果自动派生)
#   4. 工作区文件树(与素材无关,任何会话都能点)
#
# 用法:pwsh scripts/make-file-reader-fixture.ps1 [-Home <数据目录>]
[CmdletBinding()]
param(
  [string]$DataHome = (Join-Path $HOME '.denia'),
  [string]$Workspace = (Split-Path $PSScriptRoot -Parent),
  [string]$Id = 'ffffffff-0000-4000-8000-00000000f11e'
)

$ErrorActionPreference = 'Stop'
$dir = Join-Path $DataHome "sessions\$Id"
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$file = Join-Path $dir 'session.jsonl'

$now = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$cwd = $Workspace

# 逐行追加(与后端 append-only 一致)。
$lines = [System.Collections.Generic.List[string]]::new()
function Add-Event($obj) { $lines.Add(($obj | ConvertTo-Json -Compress -Depth 12)) }

Add-Event @{ type='session'; version=0; id=$Id; created_at=$now; cwd=$cwd; sandbox=$true }
Add-Event @{ seq=1; time=$now; type='permission-mode'; mode='auto-edit' }
Add-Event @{ seq=2; time=$now; type='agent-preset'; preset='standard' }
Add-Event @{ seq=3; time=$now; type='user-message'; text='文件读取标签页验收素材'; injected=$false }

Add-Event @{ seq=4; time=$now; type='turn-start'; turn=1 }
Add-Event @{ seq=5; time=$now; type='step-start'; turn=1; step=1 }

# 助手消息:包含四类链接 ——
#   - 工作区内相对路径(应被接手,在文件读取标签页打开)
#   - 带 ./ 前缀的相对路径(归一化后与上者同一文件)
#   - 带反斜杠的路径(Windows 形态)
#   - 外部 http(s) URL(必须保持默认行为,不受影响)
$md = @'
### 文件引用素材

- 普通相对路径:[Cargo.toml](Cargo.toml)
- 带 `./` 前缀:[AGENTS.md](./AGENTS.md)
- 反斜杠形态:[fs.rs](crates\server\src\api\fs.rs)
- 外部链接(应保持默认跳转):[DeepSeek](https://www.deepseek.com/)
- 不存在的文件(应显示失败提示):[missing](does/not/exist.txt)
'@
Add-Event @{ seq=6; time=$now; type='assistant-message'; turn=1; step=1; blocks=@(
  @{ type='text'; text=$md }
) }

Add-Event @{ seq=7; time=$now; type='step-end'; turn=1; step=1 }
Add-Event @{ seq=8; time=$now; type='step-start'; turn=1; step=2 }

# 工具节点由**独立的 tool-call 事件**注册(见 web/src/fold.ts 的 case 'tool-call'),
# 不是从 assistant-message 的 blocks 里推的 —— 两条都写,才能既渲染助手正文
# 又建出可点的工具行。
$editArgs = '{"path":"web/src/sidePane.ts","old_string":"export const RECENT_CLOSED_LIMIT = 8","new_string":"export const RECENT_CLOSED_LIMIT = 8"}'
Add-Event @{ seq=9; time=$now; type='tool-call'; turn=1; step=2; call_id='call_fixture_edit'; name='edit'; arguments=$editArgs }
Add-Event @{ seq=10; time=$now; type='assistant-message'; turn=1; step=2; blocks=@(
  @{ type='text'; text='先改一处。' }
  @{ type='tool-call'; id='call_fixture_edit'; name='edit'; arguments=$editArgs }
) }
Add-Event @{ seq=11; time=$now; type='tool-result'; turn=1; step=2; call_id='call_fixture_edit'; content='已替换 1 处'; is_error=$false }

Add-Event @{ seq=12; time=$now; type='step-end'; turn=1; step=2 }
Add-Event @{ seq=13; time=$now; type='step-start'; turn=1; step=3 }

# write_file 工具调用:路径可点,且它会让本轮产物列表出现(触发点 3)。
$writeArgs = '{"path":"docs/file-reader-fixture.md","content":"# 验收素材"}'
Add-Event @{ seq=14; time=$now; type='tool-call'; turn=1; step=3; call_id='call_fixture_write'; name='write_file'; arguments=$writeArgs }
Add-Event @{ seq=15; time=$now; type='assistant-message'; turn=1; step=3; blocks=@(
  @{ type='text'; text='再写一份说明。' }
  @{ type='tool-call'; id='call_fixture_write'; name='write_file'; arguments=$writeArgs }
) }
Add-Event @{ seq=16; time=$now; type='tool-result'; turn=1; step=3; call_id='call_fixture_write'; content='已写入 docs/file-reader-fixture.md'; is_error=$false }

Add-Event @{ seq=17; time=$now; type='step-end'; turn=1; step=3 }
Add-Event @{ seq=18; time=$now; type='assistant-message'; turn=1; step=3; blocks=@(
  @{ type='text'; text='素材准备好了。' }
) }
Add-Event @{ seq=19; time=$now; type='turn-end'; turn=1; reason=@{ kind='completed' } }

# 注意:不写 request-header / system-prompt —— 那些体积大且与本次验收无关,
# 会话页不依赖它们渲染对话流。

Set-Content -Path $file -Value $lines -Encoding utf8NoBOM
Write-Host "fixture session written:"
Write-Host "  file:      $file"
Write-Host "  session:   $Id"
Write-Host "  cwd:       $cwd"
Write-Host "  open at:   http://127.0.0.1:3601/#s=$Id"
