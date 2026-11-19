(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.LocmAgentWorkspace = api;
})(globalThis, function () {
  function formatWorkspaceForDisplay(path) {
    const value = String(path || "");
    const uncPrefix = "\\\\?\\UNC\\";
    if (value.slice(0, uncPrefix.length).toUpperCase() === uncPrefix.toUpperCase()) {
      return `\\\\${value.slice(uncPrefix.length)}`;
    }
    return value.startsWith("\\\\?\\") ? value.slice(4) : value;
  }

  function displayState(task, defaultWorkspace) {
    const selectedDefault = String(defaultWorkspace || "");
    const taskWorkspace = String(task?.workspace || "");
    if (!taskWorkspace) {
      return {
        primaryLabel: "新任务默认工作区",
        primaryPath: formatWorkspaceForDisplay(selectedDefault || "尚未选择"),
        secondaryText: "",
        showSecondary: false,
      };
    }
    return {
      primaryLabel: "当前任务绑定目录",
      primaryPath: formatWorkspaceForDisplay(taskWorkspace),
      secondaryText: selectedDefault === taskWorkspace
        ? "新任务默认目录：与当前任务相同"
        : `新任务默认目录：${formatWorkspaceForDisplay(selectedDefault || "尚未选择")}`,
      showSecondary: true,
    };
  }

  function isUnstartedTask(task) {
    return Boolean(task)
      && !(task.entries?.length)
      && !(task.turns?.length)
      && !(task.execution?.steps?.length)
      && !task.pendingAction
      && task.status !== "running"
      && task.status !== "executing_approved";
  }

  function applySelection(task, selectedWorkspace) {
    const canonicalWorkspace = String(selectedWorkspace || "");
    return {
      defaultWorkspace: canonicalWorkspace,
      updateTaskWorkspace: isUnstartedTask(task),
      taskWorkspace: isUnstartedTask(task) ? canonicalWorkspace : (task?.workspace || ""),
    };
  }

  function workspaceForNewTask(defaultWorkspace) {
    return String(defaultWorkspace || "");
  }

  return { displayState, formatWorkspaceForDisplay, isUnstartedTask, applySelection, workspaceForNewTask };
});
