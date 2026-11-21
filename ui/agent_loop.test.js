const test = require("node:test");
const assert = require("node:assert/strict");
const AgentLoop = require("./agent_loop.js");
const Protocol = require("./agent_protocol.js");

function task() {
  return {
    status: "running",
    needsPlan: false,
    plan: [],
    turns: [{ role: "user", content: "读取 note.txt 并总结" }],
  };
}

function dependencies(currentTask, modelReplies, observed = {}) {
  let stopped = false;
  const deps = {
    maxSteps: 12,
    isStopRequested: () => stopped,
    requestModel: async (activeTask) => {
      observed.requests = (observed.requests || 0) + 1;
      observed.turnSnapshots ||= [];
      observed.turnSnapshots.push(structuredClone(activeTask.turns));
      return modelReplies.shift();
    },
    parseResponse: Protocol.parseAgentJson,
    parseToolAction: (parsed) => Protocol.parseToolAction(parsed, ["read_file"]),
    executeToolAction: async (activeTask, action) => {
      observed.actions = [...(observed.actions || []), action];
      const result = `工作区工具结果：${action.path}\n文件内容：已生成的实测模拟内容`;
      Protocol.appendToolResult(activeTask, result);
      observed.toolResults = [...(observed.toolResults || []), result];
      return "continue";
    },
    isClarificationRequest: Protocol.isClarificationRequest,
    appendMalformedResponseRetry: Protocol.appendMalformedResponseRetry,
    processPlanResponse: (activeTask, raw, parsed, options) => {
      observed.planOptions = options;
      return parsed.type === "plan" ? "handled" : "not_plan";
    },
    onStopped: () => { observed.stopCallback = true; },
    onClarification: () => { observed.clarification = true; },
    onMalformedResponse: (activeTask, raw) => activeTask.turns.push({ role: "assistant", content: raw }),
    onMalformedRetry: () => { observed.malformedRetry = true; },
    onMalformedExhausted: () => { observed.malformedExhausted = true; },
    onPlanHandled: () => { observed.planHandled = (observed.planHandled || 0) + 1; },
    onDone: (activeTask, parsed) => { activeTask.status = "completed"; observed.done = parsed.message; },
    onMessage: (activeTask, raw, parsed, message) => {
      activeTask.turns.push({ role: "assistant", content: raw });
      Protocol.appendContinuation(activeTask, message);
    },
    onInvalidToolRequest: () => { observed.invalidTool = (observed.invalidTool || 0) + 1; },
    onInvalidToolExhausted: () => { observed.invalidToolExhausted = true; },
    onUnsupportedResponse: () => { observed.unsupported = true; },
    onToolAction: () => {},
    onToolPaused: () => {},
    onToolContinued: () => { observed.continued = (observed.continued || 0) + 1; },
    onStepLimit: (_activeTask, maxSteps) => { observed.stepLimit = maxSteps; },
  };
  return { deps, setStopped: (value) => { stopped = value; } };
}

test("model tool action executes and its workspace result reaches the next model request", async () => {
  const currentTask = task();
  const observed = {};
  const { deps } = dependencies(currentTask, [
    JSON.stringify({ type: "tool_call", name: "read_file", path: "note.txt" }),
    JSON.stringify({ type: "done", message: "note.txt 的内容已读取并总结。" }),
  ], observed);

  const result = await AgentLoop.run(currentTask, deps);

  assert.equal(result, "completed");
  assert.equal(currentTask.status, "completed");
  assert.equal(observed.requests, 2);
  assert.deepEqual(observed.actions, [{ name: "read_file", path: "note.txt" }]);
  assert.equal(observed.continued, 1);
  const secondRequestTurns = observed.turnSnapshots[1];
  const toolResultTurn = secondRequestTurns.find((turn) => turn.content.includes(Protocol.TOOL_RESULT_PREFIX));
  assert.ok(toolResultTurn, "second model request must contain the internal tool-result turn");
  assert.match(toolResultTurn.content, /实测模拟内容/);
  assert.equal(observed.done, "note.txt 的内容已读取并总结。");
});

test("malformed model output gets only one correction retry before safe pause", async () => {
  const currentTask = task();
  const observed = {};
  const { deps } = dependencies(currentTask, ["not json", "still not json"], observed);

  const result = await AgentLoop.run(currentTask, deps);

  assert.equal(result, "paused");
  assert.equal(observed.requests, 2);
  assert.equal(observed.malformedRetry, true);
  assert.equal(observed.malformedExhausted, true);
  assert.equal(observed.actions, undefined);
});

test("an invalid placeholder plan gets one correction retry and then pauses without tools", async () => {
  const currentTask = task();
  currentTask.needsPlan = true;
  const observed = {};
  const { deps } = dependencies(currentTask, [
    '{"type":"plan","steps":["步骤一","步骤二"]}',
    '{"type":"plan","steps":["步骤一","步骤二"]}',
    JSON.stringify({ type: "tool_call", name: "read_file", path: "note.txt" }),
  ], observed);
  let validations = 0;
  deps.processPlanResponse = async () => {
    validations += 1;
    return validations === 1 ? "retry_invalid" : "invalid";
  };

  const result = await AgentLoop.run(currentTask, deps);

  assert.equal(result, "paused");
  assert.equal(observed.requests, 2);
  assert.equal(validations, 2);
  assert.equal(observed.actions, undefined);
});

test("normalizes the exact list_files .** alias to the root and never executes a glob", async () => {
  const currentTask = task();
  const observed = {};
  const { deps } = dependencies(currentTask, [
    JSON.stringify({ type: "tool_call", name: "list_files", path: ".**" }),
    JSON.stringify({ type: "done", message: "已检查工作区。" }),
  ], observed);
  deps.parseToolAction = (parsed) => Protocol.parseToolAction(parsed, ["list_files"]);

  const result = await AgentLoop.run(currentTask, deps);

  assert.equal(result, "completed");
  assert.equal(observed.requests, 2);
  assert.equal(observed.invalidTool, undefined);
  assert.deepEqual(observed.actions, [{ name: "list_files", path: ".", correctedFrom: ".**" }]);
});

test("pauses after a repeated invalid tool request without executing it", async () => {
  const currentTask = task();
  const observed = {};
  const invalid = JSON.stringify({ type: "tool_call", name: "read_file", path: ".**" });
  const { deps } = dependencies(currentTask, [invalid, invalid], observed);

  const result = await AgentLoop.run(currentTask, deps);

  assert.equal(result, "paused");
  assert.equal(observed.requests, 2);
  assert.equal(observed.invalidTool, 2);
  assert.equal(observed.invalidToolExhausted, true);
  assert.equal(observed.actions, undefined);
});

test("stop requested while the model is responding prevents its tool action from executing", async () => {
  const currentTask = task();
  const observed = {};
  const { deps, setStopped } = dependencies(currentTask, [
    JSON.stringify({ type: "tool_call", name: "read_file", path: "note.txt" }),
  ], observed);
  const requestModel = deps.requestModel;
  deps.requestModel = async (...args) => {
    const reply = await requestModel(...args);
    setStopped(true);
    return reply;
  };
  deps.onStopped = (_activeTask, phase) => { observed.stopPhase = phase; };

  const result = await AgentLoop.run(currentTask, deps);

  assert.equal(result, "stopped");
  assert.equal(observed.stopPhase, "after_response");
  assert.equal(observed.actions, undefined);
});
