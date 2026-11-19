const test = require("node:test");
const assert = require("node:assert/strict");
const { removeTaskAndPersist } = require("./agent_history.js");

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
