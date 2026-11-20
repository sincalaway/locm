const test = require("node:test");
const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

const html = readFileSync(join(__dirname, "index.html"), "utf8");
const agent = readFileSync(join(__dirname, "agent.js"), "utf8");

test("Agent improvement UI is local, reviewable, and explicit that approval does not apply changes", () => {
  assert.match(html, /id="agent-improvement-panel"/);
  assert.match(html, /id="agent-proposal-form"/);
  assert.match(html, /id="agent-run-benchmark"/);
  assert.match(html, /批准只表示可供后续开发实施，不会应用到提示词、工具权限、工具清单或程序文件/);
  assert.match(agent, /批准供开发实现/);
  assert.match(agent, /运行配置和程序均未更改/);
});

test("benchmark UI accurately says it does not run the local model or verify Windows end to end", () => {
  assert.match(html, /不启动本地模型、不调用工作区工具，也不验证 Windows GUI/);
  assert.match(html, /当前代码基线，不是候选改动的测试/);
  assert.match(html, /不能证明模型能力或 Windows 端到端行为/);
  assert.match(html, /src="\.\/agent_proposals\.js"/);
  assert.match(html, /src="\.\/agent_benchmark\.js"/);
});
