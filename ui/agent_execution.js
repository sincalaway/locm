(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.LocmAgentExecution = api;
})(globalThis, function () {
  const MAX_STEPS = 120;
  const STEP_STATUSES = new Set(["pending", "running", "completed", "failed", "interrupted"]);
  const RECOVERY_MODES = new Set(["plan_only", "awaiting_user_instruction"]);

  function emptyExecution() {
    return { version: 1, steps: [], currentStepId: null, recovery: null };
  }

  function normalizeStep(step) {
    if (!step || typeof step !== "object" || typeof step.id !== "string" || !STEP_STATUSES.has(step.status)) return null;
    if (!["model", "tool", "checkpoint"].includes(step.kind)) return null;
    return {
      id: step.id,
      kind: step.kind,
      requestId: typeof step.requestId === "string" ? step.requestId : null,
      label: typeof step.label === "string" ? step.label.slice(0, 500) : "未命名步骤",
      status: step.status,
      startedAt: Number.isFinite(step.startedAt) ? step.startedAt : Date.now(),
      finishedAt: Number.isFinite(step.finishedAt) ? step.finishedAt : null,
      result: typeof step.result === "string" ? step.result.slice(0, 8000) : "",
      toolName: typeof step.toolName === "string" ? step.toolName.slice(0, 80) : null,
      risk: step.risk === "side_effect" ? "side_effect" : step.risk === "read_only" ? "read_only" : null,
    };
  }

  function normalizeExecution(value) {
    if (!value || typeof value !== "object") return emptyExecution();
    const steps = Array.isArray(value.steps) ? value.steps.map(normalizeStep).filter(Boolean).slice(-MAX_STEPS) : [];
    const ids = new Set(steps.map((step) => step.id));
    const currentStepId = typeof value.currentStepId === "string" && ids.has(value.currentStepId) ? value.currentStepId : null;
    const recovery = value.recovery && RECOVERY_MODES.has(value.recovery.mode)
      ? {
        mode: value.recovery.mode,
        reason: typeof value.recovery.reason === "string" ? value.recovery.reason.slice(0, 1000) : "恢复前的结果未知。",
      }
      : null;
    return { version: 1, steps, currentStepId, recovery };
  }

  function appendStep(task, fields, now = Date.now()) {
    task.execution = normalizeExecution(task.execution);
    const step = {
      id: String(fields.id || `${now}-${Math.random().toString(16).slice(2)}`),
      kind: fields.kind,
      requestId: typeof fields.requestId === "string" ? fields.requestId : typeof task.requestId === "string" ? task.requestId : null,
      label: String(fields.label || "未命名步骤").slice(0, 500),
      status: fields.status || "running",
      startedAt: Number.isFinite(fields.startedAt) ? fields.startedAt : now,
      finishedAt: null,
      result: String(fields.result || ""),
      toolName: typeof fields.toolName === "string" ? fields.toolName : null,
      risk: fields.risk === "side_effect" ? "side_effect" : fields.risk === "read_only" ? "read_only" : null,
    };
    task.execution.steps.push(step);
    task.execution.steps = task.execution.steps.slice(-MAX_STEPS);
    task.execution.currentStepId = step.status === "running" || step.status === "pending" ? step.id : null;
    return step;
  }

  function beginStep(task, fields, now = Date.now()) {
    task.execution = normalizeExecution(task.execution);
    const active = task.execution.steps.find((step) => step.status === "running");
    if (active) finishStep(task, active.id, "interrupted", "步骤被后续执行替代；结果需按未知处理。", now);
    return appendStep(task, { ...fields, status: "running", startedAt: now }, now);
  }

  function updateStep(task, stepId, status, result, now = Date.now()) {
    task.execution = normalizeExecution(task.execution);
    if (!STEP_STATUSES.has(status)) throw new Error(`不支持的执行步骤状态：${status}`);
    const step = task.execution.steps.find((item) => item.id === stepId);
    if (!step) return null;
    step.status = status;
    step.result = String(result ?? "").slice(0, 8000);
    step.finishedAt = status === "pending" || status === "running" ? null : now;
    task.execution.currentStepId = status === "pending" || status === "running" ? step.id : null;
    return step;
  }

  function markInterrupted(task, now = Date.now(), fallbackLabel = "应用关闭时的未记录步骤") {
    task.execution = normalizeExecution(task.execution);
    let step = task.execution.steps.find((item) => item.id === task.execution.currentStepId && item.status === "running")
      || task.execution.steps.find((item) => item.status === "running");
    if (!step) {
      step = appendStep(task, {
        kind: task.status === "executing_approved" ? "tool" : "checkpoint",
        label: fallbackLabel,
        status: "running",
        toolName: task.status === "executing_approved" ? "unknown" : null,
        risk: task.status === "executing_approved" ? "side_effect" : null,
      }, now);
    }
    const result = step.kind === "tool" && step.risk === "side_effect"
      ? "应用关闭时操作结果未知；可能已部分或全部产生副作用。不会自动重放，也不会跳过原有确认。请先检查工作区及相关进程。"
      : "应用关闭时步骤结果未知。恢复只会使用已有对话上下文重新生成计划，不会自动执行任何工具。";
    updateStep(task, step.id, "interrupted", result, now);
    return step;
  }

  function recoverLoadedTask(task, now = Date.now()) {
    const hadExecution = Boolean(task.execution && typeof task.execution === "object");
    task.execution = normalizeExecution(task.execution);
    const wasApprovedExecution = task.status === "executing_approved";
    const wasRunning = task.status === "running";
    const hasRunningStep = task.execution.steps.some((step) => step.status === "running");
    if (!wasApprovedExecution && !wasRunning && !hasRunningStep) {
      if (!hadExecution) task.execution = emptyExecution();
      if (task.status === "awaiting_approval" && task.pendingAction) {
        const linked = task.execution.steps.some((step) => step.id === task.pendingAction.stepId && step.status === "pending");
        if (!linked) {
          const action = task.pendingAction;
          const label = action.kind === "command"
            ? `等待确认：运行 ${JSON.stringify([action.program, ...(action.args || [])])}`
            : action.kind === "delete" ? `等待确认：删除 ${action.path}` : `等待确认：覆盖 ${action.path}`;
          const step = appendStep(task, {
            kind: "tool", label, status: "pending",
            toolName: action.kind === "command" ? "run_command" : action.kind === "delete" ? "delete_file" : "write_file",
            risk: "side_effect", result: "等待用户确认；尚未执行。",
          }, now);
          action.stepId = step.id;
        }
      }
      return { changed: !hadExecution, notice: null };
    }

    const legacy = !hadExecution || task.execution.steps.length === 0;
    markInterrupted(task, now, legacy ? "旧任务历史中的未记录执行阶段" : "应用关闭时的步骤");
    task.status = "paused";
    if (wasApprovedExecution || hasRunningStep) task.pendingAction = null;
    const reason = wasApprovedExecution
      ? "已批准操作在应用关闭时中断，结果可能未知；不会自动重跑。请检查实际结果后，再决定是否发出新指示。"
      : legacy
        ? "旧历史没有逐步执行记录，无法确定上次停在模型还是工具；不会重放工具。点击下方按钮只用已有上下文重新生成计划。"
        : "上次任务在此步骤中断；操作结果可能未知。不会自动重放工具。点击下方按钮只用已有上下文重新生成计划。";
    task.execution.recovery = { mode: "plan_only", reason };
    return { changed: true, notice: reason };
  }

  function interruptTask(task, now = Date.now()) {
    if (!task || task.status !== "running" && task.status !== "executing_approved") return null;
    const wasApprovedExecution = task.status === "executing_approved";
    const step = markInterrupted(task, now, "应用关闭时的执行阶段");
    task.status = "paused";
    if (wasApprovedExecution) task.pendingAction = null;
    task.execution.recovery = {
      mode: "plan_only",
      reason: wasApprovedExecution
        ? "已批准操作中断，结果未知或可能部分完成；不会自动重跑。请先检查实际结果。"
        : "应用关闭时正在执行的步骤已标为中断；恢复时只用已有上下文重新生成计划，不会自动执行工具。",
    };
    return step;
  }

  function setRecoveryMode(task, mode, reason) {
    task.execution = normalizeExecution(task.execution);
    if (!RECOVERY_MODES.has(mode)) throw new Error(`不支持的恢复状态：${mode}`);
    task.execution.recovery = { mode, reason: String(reason || "").slice(0, 1000) };
  }

  function clearRecovery(task) {
    task.execution = normalizeExecution(task.execution);
    task.execution.recovery = null;
  }

  function toolsBlocked(task) {
    return Boolean(task?.execution?.recovery);
  }

  function stepsForRequest(value, requestId) {
    const steps = normalizeExecution(value).steps;
    return typeof requestId === "string" ? steps.filter((step) => step.requestId === requestId) : steps;
  }

  return {
    MAX_STEPS,
    appendStep,
    beginStep,
    clearRecovery,
    emptyExecution,
    interruptTask,
    markInterrupted,
    normalizeExecution,
    recoverLoadedTask,
    setRecoveryMode,
    stepsForRequest,
    toolsBlocked,
    updateStep,
  };
});
