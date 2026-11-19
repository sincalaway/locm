const test = require("node:test");
const assert = require("node:assert/strict");
const {
  beginStep,
  clearRecovery,
  emptyExecution,
  interruptTask,
  recoverLoadedTask,
  toolsBlocked,
  updateStep,
} = require("./agent_execution.js");

function makeTask(overrides = {}) {
  return {
    id: "task-1",
    status: "running",
    turns: [{ role: "user", content: "整理项目" }],
    entries: [{ kind: "user", text: "整理项目" }],
    pendingAction: null,
    execution: emptyExecution(),
    ...overrides,
  };
}

test("records completed model and tool steps with their results", () => {
  const task = makeTask();
  const model = beginStep(task, { kind: "model", label: "本地模型请求" }, 10);
  updateStep(task, model.id, "completed", '{"type":"plan","steps":["检查文件"]}', 11);
  const tool = beginStep(task, { kind: "tool", label: "读取 README.md", toolName: "read_file", risk: "read_only" }, 12);
  updateStep(task, tool.id, "completed", "已读取 README.md：说明文本", 13);

  assert.deepEqual(task.execution.steps.map(({ status }) => status), ["completed", "completed"]);
  assert.match(task.execution.steps[0].result, /plan/);
  assert.match(task.execution.steps[1].result, /说明文本/);
  assert.equal(task.execution.currentStepId, null);
});

test("records a tool failure without converting it to success", () => {
  const task = makeTask();
  const tool = beginStep(task, { kind: "tool", label: "运行命令", toolName: "run_command", risk: "side_effect" }, 20);
  updateStep(task, tool.id, "failed", "执行失败；实际副作用需检查。", 21);

  assert.equal(task.execution.steps[0].status, "failed");
  assert.match(task.execution.steps[0].result, /需检查/);
  assert.equal(task.execution.currentStepId, null);
});

test("marks an in-flight side-effect tool interrupted on exit and restores it without replay", () => {
  const task = makeTask();
  const tool = beginStep(task, {
    kind: "tool",
    label: "运行命令：python script.py",
    toolName: "run_command",
    risk: "side_effect",
  }, 30);
  const interrupted = interruptTask(task, 31);

  assert.equal(interrupted.id, tool.id);
  assert.equal(task.status, "paused");
  const interruptedStep = task.execution.steps.find((step) => step.id === tool.id);
  assert.equal(interruptedStep.status, "interrupted");
  assert.match(interruptedStep.result, /不会自动重放/);
  assert.equal(task.execution.recovery.mode, "plan_only");

  const restored = JSON.parse(JSON.stringify(task));
  const recovery = recoverLoadedTask(restored, 32);
  assert.equal(restored.execution.steps[0].status, "interrupted");
  assert.equal(restored.execution.recovery.mode, "plan_only");
  assert.equal(toolsBlocked(restored), true);
  assert.equal(recovery.notice, null);
  assert.match(restored.execution.recovery.reason, /只用已有上下文重新生成计划/);
  clearRecovery(restored);
  assert.equal(toolsBlocked(restored), false);
});

test("migrates running legacy task history without changing its existing turns or entries", () => {
  const legacy = {
    id: "legacy-running",
    status: "running",
    turns: [{ role: "user", content: "旧任务" }, { role: "assistant", content: "旧回复" }],
    entries: [{ kind: "user", text: "旧任务" }, { kind: "assistant", text: "旧回复" }],
    pendingAction: null,
  };
  const turnsBefore = structuredClone(legacy.turns);
  const entriesBefore = structuredClone(legacy.entries);
  const result = recoverLoadedTask(legacy, 40);

  assert.equal(legacy.status, "paused");
  assert.equal(legacy.execution.steps[0].status, "interrupted");
  assert.equal(legacy.execution.recovery.mode, "plan_only");
  assert.deepEqual(legacy.turns, turnsBefore);
  assert.deepEqual(legacy.entries, entriesBefore);
  assert.match(result.notice, /旧历史没有逐步执行记录/);
});

test("recovers a persisted running side-effect step after an unclean shutdown", () => {
  const active = makeTask();
  const step = beginStep(active, { kind: "tool", label: "覆盖 settings.json", toolName: "write_file", risk: "side_effect" }, 45);
  const restored = JSON.parse(JSON.stringify(active));
  const recovery = recoverLoadedTask(restored, 46);

  assert.equal(restored.status, "paused");
  assert.equal(restored.execution.steps[0].id, step.id);
  assert.equal(restored.execution.steps[0].status, "interrupted");
  assert.match(restored.execution.steps[0].result, /可能已部分或全部产生副作用/);
  assert.equal(restored.execution.recovery.mode, "plan_only");
  assert.equal(toolsBlocked(restored), true);
  assert.match(recovery.notice, /不会自动重放工具/);
});

test("keeps old completed task history readable and adds only an empty execution record", () => {
  const legacy = {
    id: "legacy-complete",
    status: "completed",
    turns: [{ role: "user", content: "已完成任务" }],
    entries: [{ kind: "user", text: "已完成任务" }],
  };
  const result = recoverLoadedTask(legacy, 50);

  assert.equal(result.notice, null);
  assert.equal(legacy.status, "completed");
  assert.deepEqual(legacy.execution.steps, []);
  assert.deepEqual(legacy.turns, [{ role: "user", content: "已完成任务" }]);
});

test("preserves a legacy pending approval as pending and links it to a visible step", () => {
  const legacy = {
    id: "legacy-approval",
    status: "awaiting_approval",
    turns: [{ role: "user", content: "运行检查命令" }],
    entries: [{ kind: "user", text: "运行检查命令" }],
    pendingAction: { kind: "command", program: "cargo", args: ["test"] },
  };
  recoverLoadedTask(legacy, 55);

  assert.equal(legacy.status, "awaiting_approval");
  assert.equal(legacy.execution.recovery, null);
  assert.equal(legacy.execution.steps[0].status, "pending");
  assert.equal(legacy.pendingAction.stepId, legacy.execution.steps[0].id);
  assert.equal(toolsBlocked(legacy), false);
});

test("treats interrupted previously approved operations as unknown and not pending approval", () => {
  const task = makeTask({ status: "executing_approved", pendingAction: null });
  const tool = beginStep(task, { kind: "tool", label: "覆盖 config.toml", toolName: "write_file", risk: "side_effect" }, 60);
  const restored = JSON.parse(JSON.stringify(task));
  restored.status = "executing_approved";
  const result = recoverLoadedTask(restored, 61);

  assert.equal(restored.status, "paused");
  assert.equal(restored.pendingAction, null);
  assert.equal(restored.execution.steps.find((step) => step.id === tool.id).status, "interrupted");
  assert.equal(toolsBlocked(restored), true);
  assert.match(result.notice, /结果可能未知/);
});
