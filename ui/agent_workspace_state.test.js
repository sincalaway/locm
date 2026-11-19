const test = require("node:test");
const assert = require("node:assert/strict");
const { applySelection, displayState, formatWorkspaceForDisplay, isUnstartedTask, workspaceForNewTask } = require("./agent_workspace_state.js");

test("makes the current task binding and new-task default distinct in the picker", () => {
  const task = { workspace: "D:\\抓取" };
  const view = displayState(task, "D:\\workspace");

  assert.equal(view.primaryLabel, "当前任务绑定目录");
  assert.equal(view.primaryPath, "D:\\抓取");
  assert.equal(view.secondaryText, "新任务默认目录：D:\\workspace");
  assert.equal(view.showSecondary, true);
});

test("without an open task, the selected folder is shown as the new-task workspace", () => {
  assert.deepEqual(displayState(null, "D:\\workspace"), {
    primaryLabel: "新任务默认工作区",
    primaryPath: "D:\\workspace",
    secondaryText: "",
    showSecondary: false,
  });
  assert.equal(displayState(null, "").primaryPath, "尚未选择");
});

test("changing the default never rebinds a task with history or pending work", () => {
  const historical = {
    workspace: "D:\\抓取",
    entries: [{ kind: "user", text: "旧任务" }],
    turns: [{ role: "user", content: "旧任务" }],
    execution: { steps: [] },
    status: "paused",
  };
  const selection = applySelection(historical, "D:\\workspace");

  assert.equal(selection.defaultWorkspace, "D:\\workspace");
  assert.equal(selection.updateTaskWorkspace, false);
  assert.equal(selection.taskWorkspace, "D:\\抓取");
  assert.equal(historical.workspace, "D:\\抓取");
  assert.equal(workspaceForNewTask(selection.defaultWorkspace), "D:\\workspace");
});

test("only an empty, unstarted draft can inherit a newly selected directory", () => {
  const draft = { workspace: "D:\\old", entries: [], turns: [], execution: { steps: [] }, status: "paused" };
  assert.equal(isUnstartedTask(draft), true);
  assert.deepEqual(applySelection(draft, "D:\\new"), {
    defaultWorkspace: "D:\\new",
    updateTaskWorkspace: true,
    taskWorkspace: "D:\\new",
  });
  assert.equal(isUnstartedTask({ ...draft, execution: { steps: [{ status: "completed" }] } }), false);
  assert.equal(isUnstartedTask({ ...draft, pendingAction: { kind: "delete" } }), false);
});

test("formats Windows extended paths for display while retaining canonical paths for work", () => {
  const canonical = "\\\\?\\D:\\workspace";
  const state = { tasks: [], currentTaskId: null };
  const selection = applySelection(null, canonical);

  assert.equal(formatWorkspaceForDisplay(canonical), "D:\\workspace");
  assert.equal(displayState(null, canonical).primaryPath, "D:\\workspace");
  assert.equal(selection.defaultWorkspace, canonical);
  assert.equal(workspaceForNewTask(selection.defaultWorkspace), canonical);

  const task = { workspace: canonical };
  assert.equal(displayState(task, canonical).primaryPath, "D:\\workspace");
  assert.equal(task.workspace, canonical);
  assert.deepEqual(state, { tasks: [], currentTaskId: null });
});

test("formats extended UNC paths but leaves ordinary paths unchanged", () => {
  assert.equal(formatWorkspaceForDisplay("\\\\?\\UNC\\server\\share\\work"), "\\\\server\\share\\work");
  assert.equal(formatWorkspaceForDisplay("D:\\workspace"), "D:\\workspace");
  assert.equal(formatWorkspaceForDisplay(""), "");
});
