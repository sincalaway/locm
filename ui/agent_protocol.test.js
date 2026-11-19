const test = require("node:test");
const assert = require("node:assert/strict");
const { buildMessages } = require("./agent_context.js");
const { TOOL_RESULT_PREFIX, appendContinuation, appendDuplicatePlanRetry, recordPlanResponse, removeDuplicatePlanEntries } = require("./agent_protocol.js");

test("a plan is recorded once and the next model request gets an explicit internal continuation", () => {
  const task = { needsPlan: true, plan: [], turns: [{ role: "user", content: "检查项目并修复问题" }] };
  const parsed = { type: "plan", steps: ["检查项目", "修复问题"] };
  const first = recordPlanResponse(task, JSON.stringify(parsed), parsed, { continueAfterPlan: true });

  assert.equal(first.status, "accepted");
  assert.deepEqual(task.plan, parsed.steps);
  assert.equal(task.needsPlan, false);
  assert.deepEqual(task.turns.map(({ role }) => role), ["user", "assistant", "user"]);
  assert.match(task.turns.at(-1).content, /不是用户新请求/);
  assert.match(task.turns.at(-1).content, /不要重复计划/);
  assert.deepEqual(buildMessages("system", task.turns, 12_000).map(({ role }) => role), ["system", "user", "assistant", "user"]);
});

test("a repeated plan cannot replace or re-display the accepted plan and is separated from the next user result", () => {
  const firstPlanText = "执行计划（2 步）：\n1. 检查项目\n2. 修复问题";
  const task = {
    needsPlan: false,
    plan: ["检查项目", "修复问题"],
    entries: [{ kind: "assistant", text: firstPlanText }],
    turns: [
      { role: "user", content: "检查项目并修复问题" },
      { role: "assistant", content: JSON.stringify({ type: "plan", steps: ["检查项目", "修复问题"] }) },
      { role: "user", content: "【应用内部继续指令】请执行，不要重复计划" },
    ],
  };
  const repeated = { type: "plan", steps: ["重新规划"] };
  const result = recordPlanResponse(task, JSON.stringify(repeated), repeated);

  assert.equal(result.status, "duplicate");
  assert.deepEqual(task.plan, ["检查项目", "修复问题"]);
  assert.deepEqual(task.entries.map(({ text }) => text), [firstPlanText]);
  assert.deepEqual(task.turns.map(({ role }) => role), ["user", "assistant", "user", "assistant"]);
  assert.equal(task.turns.at(-1).content, JSON.stringify(repeated));

  // processPlanResponse adds this user-role result before pausing, keeping the
  // assistant response in the audit history without an invalid assistant suffix.
  task.turns.push({ role: "user", content: "已展示计划保持不变；重复计划未执行。" });
  const messages = buildMessages("system", task.turns, 12_000);
  assert.deepEqual(messages.map(({ role }) => role), ["system", "user", "assistant", "user", "assistant", "user"]);
  assert.equal(removeDuplicatePlanEntries([
    { kind: "assistant", text: firstPlanText },
    { kind: "assistant", text: firstPlanText },
  ]).length, 1);
});

test("a genuine new user request permits a fresh plan, while recovery can explicitly replan", () => {
  const task = { needsPlan: true, plan: ["old"], turns: [{ role: "user", content: "新的用户指示" }] };
  const parsed = { type: "plan", steps: ["新计划"] };
  assert.equal(recordPlanResponse(task, JSON.stringify(parsed), parsed).status, "accepted");
  assert.deepEqual(task.plan, ["新计划"]);

  task.needsPlan = false;
  const recovery = { type: "plan", steps: ["恢复时检查状态"] };
  assert.equal(recordPlanResponse(task, JSON.stringify(recovery), recovery, { allowReplan: true }).status, "accepted");
  assert.deepEqual(task.plan, recovery.steps);
});

test("continuations are user-role boundaries, never a second trailing assistant", () => {
  const task = { turns: [{ role: "assistant", content: "进度" }] };
  appendContinuation(task, "继续。");
  assert.deepEqual(task.turns.map(({ role }) => role), ["assistant", "user"]);
});

test("a duplicate plan gets at most one explicit correction continuation", () => {
  const task = {
    turns: [
      { role: "user", content: "检查工作区" },
      { role: "assistant", content: '{"type":"plan","steps":["列出文件"]}' },
      { role: "user", content: "【LocM 应用内部继续指令；不是用户新请求】执行计划" },
      { role: "assistant", content: '{"type":"plan","steps":["重新列计划"]}' },
    ],
  };

  const retry = appendDuplicatePlanRetry(task, 1);
  assert.deepEqual(retry, { status: "retry", retriesRemaining: 0 });
  assert.equal(task.turns.at(-1).role, "user");
  assert.match(task.turns.at(-1).content, /^【LocM 应用内部继续指令；不是用户新请求】/);
  assert.match(task.turns.at(-1).content, /不要重新生成或修改计划/);
  assert.match(task.turns.at(-1).content, /list_files/);
  const countBeforeExhaustedRetry = task.turns.length;
  assert.deepEqual(appendDuplicatePlanRetry(task, retry.retriesRemaining), { status: "exhausted", retriesRemaining: 0 });
  assert.equal(task.turns.length, countBeforeExhaustedRetry);
  assert.equal(TOOL_RESULT_PREFIX, "【LocM 应用内部工作区工具结果；不是用户新请求】");
});

test("loading historical task entries collapses only identical adjacent plan cards", () => {
  const plan = "执行计划（1 步）：\n1. 检查文件";
  const entries = [
    { kind: "assistant", text: plan },
    { kind: "assistant", text: plan },
    { kind: "tool", text: "工具结果" },
    { kind: "assistant", text: plan },
    { kind: "assistant", text: `${plan}\n不同计划` },
  ];
  const cleaned = removeDuplicatePlanEntries(entries);

  assert.deepEqual(cleaned.map(({ kind, text }) => [kind, text]), [
    ["assistant", plan],
    ["tool", "工具结果"],
    ["assistant", plan],
    ["assistant", `${plan}\n不同计划`],
  ]);
});
