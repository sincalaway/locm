(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.LocmAgentLoop = api;
})(globalThis, function () {
  async function run(task, deps) {
    const maxSteps = Number.isInteger(deps.maxSteps) && deps.maxSteps > 0 ? deps.maxSteps : 12;
    let duplicatePlanRetriesRemaining = 1;
    let invalidPlanRetriesRemaining = 1;
    let malformedProtocolRetriesRemaining = 1;
    let invalidToolRetriesRemaining = 1;

    for (let step = 0; step < maxSteps; step++) {
      if (deps.isStopRequested()) {
        await deps.onStopped(task, "before_request");
        return "stopped";
      }

      const raw = await deps.requestModel(task);
      if (deps.isStopRequested()) {
        await deps.onStopped(task, "after_response");
        return "stopped";
      }

      const parsed = deps.parseResponse(raw);
      if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
        if (deps.isClarificationRequest(raw)) {
          await deps.onClarification(task, raw);
          return "paused";
        }
        await deps.onMalformedResponse(task, raw);
        const retry = deps.appendMalformedResponseRetry(task, malformedProtocolRetriesRemaining);
        if (retry.status === "retry") {
          malformedProtocolRetriesRemaining = retry.retriesRemaining;
          await deps.onMalformedRetry(task, raw);
          continue;
        }
        await deps.onMalformedExhausted(task, raw);
        return "paused";
      }

      const planHandling = await deps.processPlanResponse(task, raw, parsed, {
        continueAfterPlan: true,
        duplicatePlanRetriesRemaining,
        invalidPlanRetriesRemaining,
      });
      if (planHandling === "retry_invalid") {
        invalidPlanRetriesRemaining -= 1;
        await deps.onPlanHandled(task, planHandling);
        continue;
      }
      if (planHandling === "invalid") {
        await deps.onPlanHandled(task, planHandling);
        return "paused";
      }
      if (planHandling === "retry_duplicate") {
        duplicatePlanRetriesRemaining -= 1;
        await deps.onPlanHandled(task, planHandling);
        continue;
      }
      if (planHandling === "duplicate") {
        await deps.onPlanHandled(task, planHandling);
        return "paused";
      }
      if (planHandling === "handled") {
        await deps.onPlanHandled(task, planHandling);
        continue;
      }

      if (parsed.type === "done") {
        await deps.onDone(task, parsed);
        return "completed";
      }

      if (parsed.type === "message") {
        const message = String(parsed.message || "");
        if (deps.isClarificationRequest(message)) {
          await deps.onClarification(task, raw, message);
          return "paused";
        }
        await deps.onMessage(task, raw, parsed, message);
        continue;
      }

      let action;
      try {
        action = deps.parseToolAction(parsed, task);
      } catch (error) {
        await deps.onInvalidToolRequest(task, raw, parsed, error);
        if (invalidToolRetriesRemaining > 0) {
          invalidToolRetriesRemaining -= 1;
          continue;
        }
        await deps.onInvalidToolExhausted(task, error);
        return "paused";
      }
      if (!action) {
        await deps.onUnsupportedResponse(task, raw, parsed);
        return "paused";
      }

      await deps.onToolAction(task, raw, parsed, action);
      const outcome = await deps.executeToolAction(task, action);
      if (["pause", "failed", "blocked"].includes(outcome)) {
        await deps.onToolPaused(task, outcome, action);
        return outcome;
      }
      await deps.onToolContinued(task, outcome, action);
    }

    await deps.onStepLimit(task, maxSteps);
    return "limit";
  }

  return { run };
});
