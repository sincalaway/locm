const test = require("node:test");
const assert = require("node:assert/strict");
const { directReply } = require("./agent_intent.js");

test("answers a simple Chinese identity question without a model or workspace", () => {
  const reply = directReply("你是谁？");
  assert.match(reply, /LocM 本地工作区 Agent/);
  assert.match(reply, /先请求确认/);
});

test("answers English identity, capability, and greeting prompts directly", () => {
  assert.match(directReply("Who are you?"), /LocM's local workspace agent/);
  assert.match(directReply("What can you do?"), /read, search, and create files/);
  assert.match(directReply("Hello!"), /What can I help you with/);
});

test("does not bypass planning for workspace requests or attached-file tasks", () => {
  assert.equal(directReply("你是谁？请读取 README.md"), null);
  assert.equal(directReply("你是谁？", true), null);
  assert.equal(directReply("帮我检查这个项目"), null);
  assert.equal(directReply(""), null);
});
