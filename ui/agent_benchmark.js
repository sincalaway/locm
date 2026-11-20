(function (root, factory) {
  const api = factory(root.LocmAgentProtocol || (typeof require === "function" ? require("./agent_protocol.js") : null),
    root.LocmAgentExecution || (typeof require === "function" ? require("./agent_execution.js") : null));
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.LocmAgentBenchmark = api;
})(globalThis, function (Protocol, Execution) {
  const SUITE_ID = "locm-agent-protocol-safety";
  const SUITE_VERSION = 1;

  function fixture(id, category, label, check) {
    try {
      const detail = check();
      return { id, category, label, passed: true, detail: String(detail || "符合预期") };
    } catch (error) {
      return { id, category, label, passed: false, detail: String(error?.message || error) };
    }
  }

  function assert(condition, detail) {
    if (!condition) throw new Error(detail);
  }

  function runSuite(ranAt = null) {
    if (!Protocol || !Execution) throw new Error("协议与执行模块尚未加载，无法运行离线基准");
    const results = [
      fixture("valid-plan", "protocol", "有效计划按协议解析", () => {
        const parsed = Protocol.parseAgentJson('{"type":"plan","steps":["检查文件","报告结果"]}');
        assert(parsed?.type === "plan" && parsed.steps.length === 2, "有效计划未被正确解析");
        return "计划 JSON 被识别；本测试不调用模型";
      }),
      fixture("prose-rejected", "protocol", "普通文本不会误识别为动作", () => {
        assert(Protocol.parseAgentJson("我已经完成了所有操作") === null, "普通文本被当作协议动作接受");
        return "纯文本被拒绝为协议对象";
      }),
      fixture("disabled-write-blocked", "safety", "关闭的写入工具在协议入口被拒绝", () => {
        let blocked = false;
        try {
          Protocol.parseToolAction({ type: "tool_call", name: "write_file", path: "note.txt", content: "x" }, []);
        } catch (error) {
          blocked = /已关闭/.test(String(error));
        }
        assert(blocked, "关闭的写入工具未被拒绝");
        return "写入请求被拒绝；没有执行文件写入";
      }),
      fixture("unknown-tool-blocked", "safety", "未知工具不会通过白名单校验", () => {
        let blocked = false;
        try { Protocol.parseToolAction({ type: "tool_call", name: "self_modify_binary" }); }
        catch (error) { blocked = /未开放的工具/.test(String(error)); }
        assert(blocked, "未知工具被接受");
        return "未知工具被拒绝；没有注册或执行工具";
      }),
      fixture("duplicate-plan-paused", "protocol", "重复计划不能替换已接受计划", () => {
        const task = { needsPlan: false, turns: [], plan: ["原计划"] };
        const outcome = Protocol.recordPlanResponse(task, "重复", { type: "plan", steps: ["新计划"] });
        assert(outcome.status === "duplicate" && task.plan[0] === "原计划", "重复计划覆盖了已接受的计划");
        return "原计划保持不变";
      }),
      fixture("interrupted-side-effect-blocked", "safety", "中断的副作用操作不会自动重放", () => {
        const task = { status: "executing_approved", pendingAction: null, execution: Execution.emptyExecution() };
        const step = Execution.beginStep(task, {
          kind: "tool", label: "测试写入", toolName: "write_file", risk: "side_effect",
        }, 100);
        Execution.interruptTask(task, 101);
        assert(task.status === "paused" && task.pendingAction === null, "中断操作恢复成待执行动作");
        assert(task.execution.steps.find((item) => item.id === step.id)?.status === "interrupted", "中断步骤状态不正确");
        assert(Execution.toolsBlocked(task), "恢复状态仍允许继续调用工具");
        return "任务暂停并阻止工具；没有执行或重放副作用";
      }),
    ];
    const passed = results.filter((result) => result.passed).length;
    const safetyChecks = results.filter((result) => result.category === "safety");
    return {
      suiteId: SUITE_ID,
      suiteVersion: SUITE_VERSION,
      total: results.length,
      passed,
      failed: results.length - passed,
      passRate: results.length ? passed / results.length : 0,
      safetyChecks: safetyChecks.length,
      safetyChecksFailed: safetyChecks.filter((result) => !result.passed).length,
      modelTested: false,
      scope: "协议与执行状态代码回归；未启动模型、未调用工作区工具、未验证 Windows GUI",
      ranAt: Number.isFinite(ranAt) ? ranAt : null,
      results,
    };
  }

  return { SUITE_ID, SUITE_VERSION, runSuite };
});
