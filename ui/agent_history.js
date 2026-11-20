(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.LocmAgentHistory = api;
})(globalThis, function () {
  function splitTranscriptEntries(entries, requestId = null) {
    const conversation = [];
    const details = [];
    for (const entry of Array.isArray(entries) ? entries : []) {
      const isPlan = entry?.kind === "assistant" && /^执行计划（\d+ 步）：/.test(entry.text || "");
      if (entry?.kind === "user" || (entry?.kind === "assistant" && !isPlan)) conversation.push(entry);
      else if (typeof requestId !== "string" || entry?.requestId === requestId) details.push(entry);
    }
    return { conversation, details };
  }

  function migrateRequestScope(task, makeId) {
    if (!task || !Array.isArray(task.entries)) return null;
    const createId = typeof makeId === "function" ? makeId : () => `${Date.now()}-${Math.random().toString(16).slice(2)}`;
    const hasRequestId = typeof task.requestId === "string" && task.requestId.length > 0;
    const steps = Array.isArray(task.execution?.steps) ? task.execution.steps : [];
    if (hasRequestId) {
      for (const entry of task.entries) {
        if (typeof entry.requestId !== "string" || !entry.requestId) entry.requestId = task.requestId;
      }
      for (const step of steps) {
        if (typeof step.requestId !== "string" || !step.requestId) step.requestId = task.requestId;
      }
      return task.requestId;
    }
    const boundaries = [];
    let activeRequestId = null;
    for (const entry of task.entries) {
      if (entry?.kind === "user") {
        activeRequestId = typeof entry.requestId === "string" && entry.requestId ? entry.requestId : createId();
        boundaries.push({ at: Number.isFinite(entry.at) ? entry.at : null, requestId: activeRequestId });
      }
      if (typeof entry.requestId === "string" && entry.requestId) activeRequestId = entry.requestId;
      else {
        activeRequestId ||= createId();
        entry.requestId = activeRequestId;
      }
    }
    activeRequestId ||= createId();
    task.requestId = activeRequestId;
    for (const step of steps) {
      if (typeof step.requestId === "string" && step.requestId) continue;
      const boundary = Number.isFinite(step.startedAt)
        ? boundaries.filter((item) => Number.isFinite(item.at) && item.at <= step.startedAt).at(-1)
        : null;
      step.requestId = boundary?.requestId || task.requestId;
    }
    return task.requestId;
  }

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

  return { migrateRequestScope, removeTaskAndPersist, splitTranscriptEntries };
});
