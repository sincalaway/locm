const test = require("node:test");
const assert = require("node:assert/strict");
const {
  CONTEXT_PRESETS,
  DEFAULT_AUTO_CONTEXT,
  MAX_MESSAGE_BYTES,
  buildMessages,
  contextBudget,
  retainHistory,
  serializedBytes,
  utf8Bytes,
} = require("./agent_context.js");
test("maps all run-page context choices to increasing input byte budgets and reserved replies", () => {
  const budgets = CONTEXT_PRESETS.map((preset) => contextBudget(String(preset)));
  assert.deepEqual(budgets.map(({ contextCapacity }) => contextCapacity), [8192, 16384, 32768, 65536]);
  assert.deepEqual(budgets.map(({ replyTokens }) => replyTokens), [1024, 2048, 4096, 4096]);
  assert.ok(budgets.every(({ inputBytes, contextCapacity, replyTokens }) => inputBytes > 0 && inputBytes < contextCapacity - replyTokens));
  assert.ok(budgets[3].inputBytes > 32 * 1024);
});
test("uses a conservative fallback when the run page is set to automatic", () => {
  assert.equal(contextBudget("auto").contextCapacity, DEFAULT_AUTO_CONTEXT);
  assert.deepEqual(contextBudget("not-a-preset"), contextBudget("auto"));
});
test("keeps the complete message history when it fits the selected byte budget", () => {
  const turns = [
    { role: "user", content: "检查这个项目" },
    { role: "assistant", content: "我会先读取 README。" },
    { role: "user", content: "工具结果：README.md 已读取" },
  ];
  const budget = contextBudget("8192");
  assert.deepEqual(buildMessages("你是本地 Agent", turns, budget.inputBytes), [
    { role: "system", content: "你是本地 Agent" },
    ...turns,
  ]);
});
test("retains the initial user goal and most recent turns within the history cap", () => {
  const turns = [{ role: "user", content: "original task goal" }];
  for (let index = 0; index < 60; index++) {
    turns.push({ role: index % 2 ? "user" : "assistant", content: `turn-${index}` });
  }
  const retained = retainHistory(turns);
  const messages = buildMessages("system", turns, 12_000);
  assert.equal(retained.length, 50);
  assert.equal(retained[0].content, "original task goal");
  assert.equal(retained.at(-1).content, "turn-59");
  assert.equal(messages[1].content, "original task goal");
  assert.equal(messages.at(-1).content, "turn-59");
  assert.ok(messages.some((message) => message.content.includes("较早的对话或工具结果已省略")));
});
test("allows a 64K selection to retain more than the old 32 KiB aggregate limit", () => {
  const turns = [{ role: "user", content: "original goal: inspect this project" }];
  for (let index = 0; index < 9; index++) {
    turns.push({ role: index % 2 ? "user" : "assistant", content: `event-${index}\n${"a".repeat(5_000)}` });
  }
  const budget = contextBudget("65536");
  const messages = buildMessages("system instructions", turns, budget.inputBytes);
  assert.ok(serializedBytes(messages) > 32 * 1024);
  assert.ok(serializedBytes(messages) <= budget.inputBytes);
  assert.equal(messages[1].content, turns[0].content);
  assert.ok(messages.at(-1).content.includes("event-8"));
});
test("bounds oversized history, preserves the task goal and newest result, and explains omissions", () => {
  const turns = [{ role: "user", content: "original goal: inspect and improve this project" }];
  for (let index = 0; index < 12; index++) {
    turns.push({
      role: index % 2 === 0 ? "assistant" : "user",
      content: `event-${index}\n${"界".repeat(5_000)}`,
    });
  }
  const budget = contextBudget("16384");
  const messages = buildMessages("system instructions", turns, budget.inputBytes);
  const contents = messages.map((message) => message.content);
  assert.ok(serializedBytes(messages) <= budget.inputBytes);
  assert.equal(messages[0].role, "system");
  assert.equal(messages[1].content, turns[0].content);
  assert.ok(contents.some((content) => content.includes("较早的对话或工具结果已省略")));
  assert.ok(contents.at(-1).includes("event-11"));
  assert.ok(!contents.some((content) => content.includes("event-0")));
});
test("truncates oversized UTF-8 messages without splitting a code point", () => {
  const budget = contextBudget("8192");
  const messages = buildMessages("system", [
    { role: "user", content: "界".repeat(15_000) },
  ], budget.inputBytes);
  const content = messages[1].content;
  assert.ok(utf8Bytes(content) <= MAX_MESSAGE_BYTES);
  assert.ok(serializedBytes(messages) <= budget.inputBytes);
  assert.ok(content.endsWith("…（消息过长，已截断）"));
  assert.ok(!content.includes("\uFFFD"));
});
