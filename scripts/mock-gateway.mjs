import http from 'node:http'

// Stateful OpenAI-compatible mock gateway:
// - no tool-role message, no todo result  -> answer with a todo_write tool call
// - todo result present, no bash result   -> answer with a bash tool call
// - both results present                  -> answer with text + stop
//
// MOCK_QUIRKS=1 模拟实测遇到过的非规范网关:tool_calls 帧省掉 index、并用
// 小写 `[done]` 收尾(都是曾让 denia 整步作废的真实形态)。用来回归流容错,
// 默认关闭,不影响常规链路测试。
const QUIRKS = process.env.MOCK_QUIRKS === '1'

// 非规范模式下剔除 index:OpenAI 规范要求每个 tool-call 增量帧都带它。
const toolCall = (obj) => {
  const out = { ...obj }
  if (QUIRKS) delete out.index
  return out
}

http
  .createServer((req, res) => {
    if (req.url?.startsWith('/v1/models')) {
      res.setHeader('content-type', 'application/json')
      res.end(JSON.stringify({ data: [{ id: 'mock-1', owned_by: 'denia-mock' }] }))
      return
    }
    if (req.url?.startsWith('/v1/chat/completions')) {
      let raw = ''
      req.on('data', (chunk) => (raw += chunk))
      req.on('end', () => {
        res.setHeader('content-type', 'text/event-stream')
        res.setHeader('cache-control', 'no-cache')
        let body = {}
        try {
          body = JSON.parse(raw)
        } catch {}
        const toolMessages = Array.isArray(body.messages)
          ? body.messages.filter((m) => m.role === 'tool')
          : []
        const sawTodo = toolMessages.some((m) => typeof m.content === 'string' && m.content.includes('todo list'))
        const sawBash = toolMessages.some((m) => typeof m.content === 'string' && m.content.includes('hi-from-mock'))

        const write = (obj) => res.write(`data: ${JSON.stringify(obj)}\n\n`)
        const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
        const streamOut = async () => {
          await sleep(2500)
        // 统一先流一段 reasoning(验证思考块自动展开/折叠)。
        write({ choices: [{ delta: { role: 'assistant' } }] })
        write({ choices: [{ delta: { reasoning_content: '让我想想这个问题……需要先理解用户意图。' } }] })
        write({ choices: [{ delta: { reasoning_content: '有一个关键点:模拟一个完整的工具调用流程才能验证链路。' } }] })
        write({ choices: [{ delta: { reasoning_content: '好的,开始执行。第一步写 todo。' } }] })
        write({ choices: [{ delta: { reasoning_content: '第二步运行 shell 命令验证工具结果回传。' } }] })
        if (!sawTodo) {
          write({ choices: [{ delta: { role: 'assistant' } }] })
          write({
            choices: [{
              delta: { tool_calls: [toolCall({ index: 0, id: 'call_todo_1', function: { name: 'todo_write', arguments: '' } })] },
            }],
          })
          write({
            choices: [{
              delta: {
                tool_calls: [toolCall({
                  index: 0,
                  function: {
                    arguments: '{"todos":[{"content":"摸清项目结构","status":"completed"},{"content":"实现核心功能","status":"in_progress"},{"content":"写测试并验证","status":"pending"}]}',
                  },
                })],
              },
            }],
          })
          write({ choices: [{ delta: {}, finish_reason: 'tool_calls' }], usage: { prompt_tokens: 20, completion_tokens: 9 } })
        } else if (!sawBash) {
          write({ choices: [{ delta: { role: 'assistant' } }] })
          write({
            choices: [{
              delta: { tool_calls: [toolCall({ index: 0, id: 'call_mock_1', function: { name: 'bash', arguments: '' } })] },
            }],
          })
          write({
            choices: [{
              delta: { tool_calls: [toolCall({ index: 0, function: { arguments: '{"command":"Write-Output hi-from-mock"}' } })] },
            }],
          })
          write({ choices: [{ delta: {}, finish_reason: 'tool_calls' }], usage: { prompt_tokens: 20, completion_tokens: 9 } })
        } else {
          write({ choices: [{ delta: { role: 'assistant' } }] })
          write({ choices: [{ delta: { content: 'the tool said: ' } }] })
          write({ choices: [{ delta: { content: 'hi from mock' } }] })
          write({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 30, completion_tokens: 7 } })
        }
        res.write(`data: ${QUIRKS ? '[done]' : '[DONE]'}\n\n`)
        res.end()
        }
        void streamOut()
      })
      return
    }
    res.statusCode = 404
    res.end('not found')
  })
  .listen(8788, '127.0.0.1', () => console.log('mock gateway on 8788'))
