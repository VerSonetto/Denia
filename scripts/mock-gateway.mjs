import http from 'node:http'

// Stateful OpenAI-compatible mock gateway:
// - no tool-role message in the request  -> answer with a bash tool call
// - tool-role message present            -> answer with text + stop
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
        const sawToolResult = Array.isArray(body.messages) &&
          body.messages.some((m) => m.role === 'tool')

        const write = (obj) => res.write(`data: ${JSON.stringify(obj)}\n\n`)
        if (!sawToolResult) {
          write({ choices: [{ delta: { role: 'assistant' } }] })
          write({
            choices: [{
              delta: { tool_calls: [{ index: 0, id: 'call_mock_1', function: { name: 'bash', arguments: '' } }] },
            }],
          })
          write({
            choices: [{
              delta: { tool_calls: [{ index: 0, function: { arguments: '{"command":"echo hi from mock"}' } }] },
            }],
          })
          write({ choices: [{ delta: {}, finish_reason: 'tool_calls' }], usage: { prompt_tokens: 20, completion_tokens: 9 } })
        } else {
          write({ choices: [{ delta: { role: 'assistant' } }] })
          write({ choices: [{ delta: { content: 'the tool said: ' } }] })
          write({ choices: [{ delta: { content: 'hi from mock' } }] })
          write({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 30, completion_tokens: 7 } })
        }
        res.write('data: [DONE]\n\n')
        res.end()
      })
      return
    }
    res.statusCode = 404
    res.end('not found')
  })
  .listen(8788, '127.0.0.1', () => console.log('mock gateway on 8788'))
