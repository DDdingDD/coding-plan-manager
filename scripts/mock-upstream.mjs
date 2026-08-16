/**
 * 本地模拟 Coding Plan 上游（用于端到端验收）
 *
 * - POST /v1/messages          Anthropic 风格（JSON / SSE 流式）
 * - POST /v1/chat/completions  OpenAI 风格（JSON / SSE 流式）
 * - 会校验收到的 Authorization / x-api-key，并打印
 * - 通过 X-MOCK-USAGE-* 请求头可自定义返回的 usage 数值
 *
 * 用法：node scripts/mock-upstream.mjs [port]   默认端口 9401
 */
import http from "node:http";

const port = Number(process.argv[2] || 9401);

const server = http.createServer((req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    const body = Buffer.concat(chunks).toString("utf8");
    const auth = req.headers["authorization"] || "";
    const apiKey = req.headers["x-api-key"] || "";
    const label = apiKey ? `x-api-key=${apiKey}` : `authorization=${auth}`;
    console.log(
      `[mock] ${new Date().toISOString()} ${req.method} ${req.url} <- ${label}`
    );

    let parsed = {};
    try {
      parsed = JSON.parse(body || "{}");
    } catch {}
    const stream = parsed.stream === true;
    const inTok = Number(req.headers["x-mock-usage-in"] || 11);
    const outTok = Number(req.headers["x-mock-usage-out"] || 7);

    const send = (status, type, payload) => {
      res.writeHead(status, { "content-type": type });
      res.end(payload);
    };

    // 错误注入：X-MOCK-STATUS: 429
    const mockStatus = Number(req.headers["x-mock-status"] || 0);
    if (mockStatus) {
      send(mockStatus, "application/json", JSON.stringify({ error: { type: "mock_error", message: "injected" } }));
      return;
    }

    if (req.url.startsWith("/v1/chat/completions")) {
      if (stream) {
        res.writeHead(200, { "content-type": "text/event-stream" });
        const parts = ["你", "好", "，", "世", "界"];
        parts.forEach((t) =>
          res.write(`data: ${JSON.stringify({ id: "c1", choices: [{ delta: { content: t } }] })}\n\n`)
        );
        res.write(
          `data: ${JSON.stringify({
            id: "c1",
            choices: [],
            usage: { prompt_tokens: inTok, completion_tokens: outTok, total_tokens: inTok + outTok },
          })}\n\n`
        );
        res.write("data: [DONE]\n\n");
        res.end();
      } else {
        send(
          200,
          "application/json",
          JSON.stringify({
            id: "c1",
            choices: [{ message: { role: "assistant", content: "你好，世界" } }],
            usage: { prompt_tokens: inTok, completion_tokens: outTok, total_tokens: inTok + outTok },
          })
        );
      }
      return;
    }

    if (req.url.startsWith("/v1/messages")) {
      if (stream) {
        res.writeHead(200, { "content-type": "text/event-stream" });
        const events = [
          ["message_start", { type: "message_start", message: { usage: { input_tokens: inTok, output_tokens: 1 } } }],
          ["content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text" } }],
          ["content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "你好" } }],
          ["content_block_stop", { type: "content_block_stop", index: 0 }],
          ["message_delta", { type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: outTok } }],
          ["message_stop", { type: "message_stop" }],
        ];
        events.forEach(([name, data]) => {
          res.write(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
        });
        res.end();
      } else {
        send(
          200,
          "application/json",
          JSON.stringify({
            id: "msg_1",
            type: "message",
            content: [{ type: "text", text: "你好，世界" }],
            usage: { input_tokens: inTok, output_tokens: outTok },
          })
        );
      }
      return;
    }

    send(404, "application/json", JSON.stringify({ error: "not found" }));
  });
});

server.listen(port, "127.0.0.1", () => {
  console.log(`[mock] coding plan 上游模拟服务已启动: http://127.0.0.1:${port}`);
  console.log("[mock] 支持: POST /v1/messages (JSON/SSE), POST /v1/chat/completions (JSON/SSE)");
});
