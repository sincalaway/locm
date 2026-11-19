(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.LocmAgentHistory = api;
})(globalThis, function () {
  function removeTaskAndPersist(state, taskId, persist, busy = false) {
    if (busy) return { status: "busy" };
    if (!state || !Array.isArray(state.tasks) || typeof taskId !== "string") {
      return { status: "not_found" };
    }
    const index = state.tasks.findIndex((task) => task && task.id === taskId);
    if (index < 0) return { status: "not_found" };

    const previousTasks = state.tasks;
    const previousCurrentTaskId = state.currentTaskId;
    const tasks = previousTasks.filter((task) => task.id !== taskId);
    const currentTaskId = previousCurrentTaskId === taskId
      ? (tasks[0]?.id || null)
      : previousCurrentTaskId;
    state.tasks = tasks;
    state.currentTaskId = currentTaskId;

    let saved = false;
    try { saved = persist() === true; } catch { /* Roll back if persistence throws. */ }
    if (!saved) {
      state.tasks = previousTasks;
      state.currentTaskId = previousCurrentTaskId;
      return { status: "persist_failed" };
    }
    return { status: "removed", taskId, currentTaskId };
  }

  return { removeTaskAndPersist };
});
