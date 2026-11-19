(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.LocmAgentProtocol = api;
})(globalThis, function () {
  const CONTINUATION_PREFIX = "【LocM 应用内部继续指令；不是用户新请求】";
  const TOOL_RESULT_PREFIX = "【LocM 应用内部工作区工具结果；不是用户新请求】";
  const encoder = new TextEncoder();

  function appendContinuation(task, instruction) {
    if (!task || !Array.isArray(task.turns)) throw new TypeError("task.turns must be an array");
    task.turns.push({
      role: "user",
      content: `${CONTINUATION_PREFIX}\n${String(instruction || "请继续处理当前任务。")}`,
    });
  }

  function appendDuplicatePlanRetry(task, retriesRemaining) {
    const remaining = Math.max(0, Math.floor(Number(retriesRemaining) || 0));
    if (!remaining) return { status: "exhausted", retriesRemaining: 0 };
    appendContinuation(task, "计划已经展示，上一条模型回复重复生成了计划。这不是用户新请求；不要重新生成或修改计划。请根据已展示计划直接执行下一步并返回对应的唯一 JSON 动作；若下一步需要检查工作区，使用 list_files；若任务已完成，直接总结。不得声称尚未执行的操作已经完成。");
    return { status: "retry", retriesRemaining: remaining - 1 };
  }

  function validatePlan(parsed) {
    const steps = Array.isArray(parsed?.steps)
      ? parsed.steps.filter((step) => typeof step === "string").map((step) => step.trim()).filter(Boolean).slice(0, 12)
      : [];
    const valid = steps.length >= 1
      && steps.length === parsed.steps.length
      && steps.every((step) => encoder.encode(step).length <= 500);
    return { valid, steps };
  }

  function recordPlanResponse(task, raw, parsed, options = {}) {
    if (parsed?.type !== "plan") return { status: "not_plan", steps: [] };
    if (!task.needsPlan && options.allowReplan !== true) {
      // Preserve the model response for audit/history. The caller adds a
      // user-role result and pauses without rendering a second plan card.
      task.turns.push({ role: "assistant", content: String(raw ?? "") });
      return { status: "duplicate", steps: [] };
    }
    const { valid, steps } = validatePlan(parsed);
    if (!valid) return { status: "invalid", steps };

    task.plan = steps;
    task.needsPlan = false;
    task.turns.push({ role: "assistant", content: JSON.stringify({ type: "plan", steps }) });
    if (options.continueAfterPlan === true) {
      appendContinuation(task, "计划已经展示。请按该计划从第一步开始执行，不要重复计划；需要文件操作时调用对应工具，完成后再总结。");
    }
    return { status: "accepted", steps };
  }

  function removeDuplicatePlanEntries(entries) {
    const result = [];
    for (const entry of Array.isArray(entries) ? entries : []) {
      const previous = result.at(-1);
      const isPlanEntry = entry?.kind === "assistant"
        && typeof entry.text === "string"
        && entry.text.startsWith("执行计划（");
      if (isPlanEntry && previous?.kind === "assistant" && previous.text === entry.text) continue;
      result.push(entry);
    }
    return result;
  }

  return { CONTINUATION_PREFIX, TOOL_RESULT_PREFIX, appendContinuation, appendDuplicatePlanRetry, validatePlan, recordPlanResponse, removeDuplicatePlanEntries };
});
