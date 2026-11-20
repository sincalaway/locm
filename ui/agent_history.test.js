const test = require("node:test");
const assert = require("node:assert/strict");
const { migrateRequestScope, removeTaskAndPersist, splitTranscriptEntries } = require("./agent_history.js");

function makeState() {
  return {
    tasks: [
      { id: "task-new", title: "new" },
      { id: "task-current", title: "current" },
      { id: "task-old", title: "old" },
    ],
    currentTaskId: "task-current",
  };
}

test("removes only the selected history item and persists tasks and current selection", () => {
  const state = makeState();
  const storage = new Map();
  const result = removeTaskAndPersist(state, "task-old", () => {
    storage.set("tasks", JSON.stringify(state.tasks));
    storage.set("current", state.currentTaskId);
    return true;
  });

  assert.equal(result.status, "removed");
  assert.deepEqual(state.tasks.map(({ id }) => id), ["task-new", "task-current"]);
  assert.equal(state.currentTaskId, "task-current");
  assert.deepEqual(JSON.parse(storage.get("tasks")).map(({ id }) => id), ["task-new", "task-current"]);
  assert.equal(storage.get("current"), "task-current");
});

test("deleting the selected task points to the newest remaining task or null", () => {
  const state = makeState();
  const result = removeTaskAndPersist(state, "task-current", () => true);

  assert.equal(result.status, "removed");
  assert.deepEqual(state.tasks.map(({ id }) => id), ["task-new", "task-old"]);
  assert.equal(state.currentTaskId, "task-new");

  const onlyTask = { tasks: [{ id: "only" }], currentTaskId: "only" };
  removeTaskAndPersist(onlyTask, "only", () => true);
  assert.deepEqual(onlyTask, { tasks: [], currentTaskId: null });
});

test("busy tasks cannot be removed and persistence is not attempted", () => {
  const state = makeState();
  let persisted = false;
  const result = removeTaskAndPersist(state, "task-old", () => { persisted = true; return true; }, true);

  assert.deepEqual(result, { status: "busy" });
  assert.equal(persisted, false);
  assert.deepEqual(state.tasks.map(({ id }) => id), ["task-new", "task-current", "task-old"]);
});

test("a missing task is a no-op rather than deleting a neighboring item", () => {
  const state = makeState();
  let persisted = false;
  const result = removeTaskAndPersist(state, "missing", () => { persisted = true; return true; });

  assert.deepEqual(result, { status: "not_found" });
  assert.equal(persisted, false);
  assert.deepEqual(state.tasks.map(({ id }) => id), ["task-new", "task-current", "task-old"]);
});

test("failed persistence rolls back in-memory history and current task id", () => {
  const state = makeState();
  const originalTasks = state.tasks;
  const result = removeTaskAndPersist(state, "task-current", () => false);

  assert.deepEqual(result, { status: "persist_failed" });
  assert.equal(state.tasks, originalTasks);
  assert.deepEqual(state.tasks.map(({ id }) => id), ["task-new", "task-current", "task-old"]);
  assert.equal(state.currentTaskId, "task-current");
});

test("keeps user requests and final replies visible while moving plan and tool events to details", () => {
  const entries = [
    { kind: "user", text: "帮我创建 ewoe 文件夹" },
    { kind: "assistant", text: "执行计划（1 步）：\n1. 在工作区创建文件夹" },
    { kind: "status", text: "正在创建工作区文件夹：ewoe" },
    { kind: "tool", text: "已创建空文件夹：ewoe" },
    { kind: "assistant", text: "已创建空文件夹：ewoe" },
  ];

  const result = splitTranscriptEntries(entries);
  assert.deepEqual(result.conversation, [entries[0], entries[4]]);
  assert.deepEqual(result.details, [entries[1], entries[2], entries[3]]);
});

test("keeps legacy non-plan messages available in their original order", () => {
  const entries = [
    { kind: "user", text: "读取文件" },
    { kind: "status", text: "旧版状态消息" },
    { kind: "assistant", text: "读取结果" },
  ];

  assert.deepEqual(splitTranscriptEntries(entries), {
    conversation: [entries[0], entries[2]],
    details: [entries[1]],
  });
});

test("shows only current-request details while preserving the full conversation", () => {
  const entries = [
    { kind: "user", text: "创建 ewoe 文件夹", requestId: "request-1" },
    { kind: "assistant", text: "执行计划（1 步）：\n1. 创建 ewoe", requestId: "request-1" },
    { kind: "tool", text: "已创建 ewoe", requestId: "request-1" },
    { kind: "assistant", text: "已创建文件夹", requestId: "request-1" },
    { kind: "user", text: "读取 /tmp/test.txt", requestId: "request-2" },
    { kind: "status", text: "正在读取文件", requestId: "request-2" },
  ];

  const result = splitTranscriptEntries(entries, "request-2");
  assert.deepEqual(result.conversation, [entries[0], entries[3], entries[4]]);
  assert.deepEqual(result.details, [entries[5]]);
});

test("migrates legacy task details to their user-request turns and selects the latest turn", () => {
  const task = {
    entries: [
      { kind: "user", text: "创建 ewoe 文件夹", at: 1 },
      { kind: "assistant", text: "执行计划（1 步）：\n1. 创建 ewoe", at: 2 },
      { kind: "tool", text: "已创建 ewoe", at: 3 },
      { kind: "assistant", text: "已创建文件夹", at: 4 },
      { kind: "user", text: "读取 /tmp/test.txt", at: 10 },
      { kind: "assistant", text: "执行计划（1 步）：\n1. 读取文件", at: 11 },
      { kind: "tool", text: "已读取文件", at: 12 },
    ],
    execution: { steps: [
      { id: "old", kind: "model", status: "completed", startedAt: 2 },
      { id: "current", kind: "model", status: "completed", startedAt: 11 },
    ] },
  };
  let nextId = 0;

  assert.equal(migrateRequestScope(task, () => `request-${++nextId}`), "request-2");
  const split = splitTranscriptEntries(task.entries, task.requestId);
  assert.deepEqual(split.details.map(({ text }) => text), [
    "执行计划（1 步）：\n1. 读取文件",
    "已读取文件",
  ]);
  assert.equal(task.execution.steps[0].requestId, "request-1");
  assert.equal(task.execution.steps[1].requestId, "request-2");
});
