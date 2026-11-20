(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.LocmAgentProposals = api;
})(globalThis, function () {
  const STORAGE_VERSION = 1;
  const BENCHMARK_SUITE_ID = "locm-agent-protocol-safety";
  const BENCHMARK_SUITE_VERSION = 1;
  const MAX_PROPOSALS = 30;
  const MAX_STORAGE_BYTES = 512 * 1024;
  const CATEGORIES = Object.freeze(["prompt", "tool_routing", "safety_policy", "other"]);
  const STATUSES = new Set(["pending_review", "approved_for_implementation", "rejected"]);
  const encoder = new TextEncoder();

  function boundedText(value, maxLength) {
    return typeof value === "string" ? value.trim().slice(0, maxLength) : "";
  }

  function normalizeBenchmarkSnapshot(value) {
    if (!value || typeof value !== "object" || value.suiteId !== BENCHMARK_SUITE_ID
      || value.suiteVersion !== BENCHMARK_SUITE_VERSION || !Number.isInteger(value.total)
      || !Number.isInteger(value.passed) || !Number.isInteger(value.failed)
      || value.total < 1 || value.passed < 0 || value.failed < 0
      || value.passed + value.failed !== value.total) return null;
    return {
      suiteId: value.suiteId.slice(0, 100),
      suiteVersion: value.suiteVersion,
      total: value.total,
      passed: value.passed,
      failed: value.failed,
      ranAt: Number.isFinite(value.ranAt) ? value.ranAt : null,
      scope: "protocol_safety_code_only",
      modelTested: false,
    };
  }

  function normalizeProposal(value) {
    if (!value || typeof value !== "object" || typeof value.id !== "string") return null;
    const benchmark = normalizeBenchmarkSnapshot(value.benchmark);
    if (!benchmark || !CATEGORIES.includes(value.category) || !STATUSES.has(value.status)) return null;
    const observedBehavior = boundedText(value.observedBehavior, 4000);
    const proposedChange = boundedText(value.proposedChange, 8000);
    const rationale = boundedText(value.rationale, 4000);
    if (!observedBehavior || !proposedChange || !rationale) return null;
    return {
      id: value.id.slice(0, 120),
      category: value.category,
      observedBehavior,
      proposedChange,
      rationale,
      taskId: typeof value.taskId === "string" ? value.taskId.slice(0, 120) : null,
      taskTitle: typeof value.taskTitle === "string" ? value.taskTitle.slice(0, 200) : null,
      benchmark,
      status: value.status,
      createdAt: Number.isFinite(value.createdAt) ? value.createdAt : 0,
      updatedAt: Number.isFinite(value.updatedAt) ? value.updatedAt : 0,
      reviewedAt: Number.isFinite(value.reviewedAt) ? value.reviewedAt : null,
    };
  }

  function normalizeProposals(value) {
    if (!Array.isArray(value)) return [];
    const seen = new Set();
    return value.map(normalizeProposal).filter((proposal) => {
      if (!proposal || seen.has(proposal.id)) return false;
      seen.add(proposal.id);
      return true;
    }).slice(0, MAX_PROPOSALS);
  }

  function loadProposals(serialized) {
    try {
      const value = typeof serialized === "string" ? JSON.parse(serialized) : serialized;
      if (!value || typeof value !== "object" || value.version !== STORAGE_VERSION) return [];
      return normalizeProposals(value.proposals);
    } catch {
      return [];
    }
  }

  function serializeProposals(proposals) {
    const normalized = normalizeProposals(proposals);
    while (normalized.length) {
      const serialized = JSON.stringify({ version: STORAGE_VERSION, proposals: normalized });
      if (encoder.encode(serialized).length <= MAX_STORAGE_BYTES) return serialized;
      normalized.pop();
    }
    return JSON.stringify({ version: STORAGE_VERSION, proposals: [] });
  }

  function createProposal(input, now = Date.now()) {
    if (!input || typeof input !== "object") throw new TypeError("提案内容无效");
    if (!CATEGORIES.includes(input.category)) throw new TypeError("提案类别无效");
    const observedBehavior = boundedText(input.observedBehavior, 4000);
    const proposedChange = boundedText(input.proposedChange, 8000);
    const rationale = boundedText(input.rationale, 4000);
    const benchmark = normalizeBenchmarkSnapshot(input.benchmark);
    if (!observedBehavior || !proposedChange || !rationale) throw new TypeError("问题证据、候选改动和预期理由都不能为空");
    if (!benchmark) throw new TypeError("请先运行并记录有效的协议/安全基准");
    return normalizeProposal({
      id: typeof input.id === "string" && input.id ? input.id : `${now}-${Math.random().toString(16).slice(2)}`,
      category: input.category,
      observedBehavior,
      proposedChange,
      rationale,
      taskId: input.taskId,
      taskTitle: input.taskTitle,
      benchmark,
      status: "pending_review",
      createdAt: now,
      updatedAt: now,
      reviewedAt: null,
    });
  }

  function addProposal(proposals, proposal) {
    const normalized = normalizeProposal(proposal);
    if (!normalized) throw new TypeError("无法保存无效提案");
    return normalizeProposals([normalized, ...normalizeProposals(proposals).filter((item) => item.id !== normalized.id)]);
  }

  function reviewProposal(proposals, id, decision, now = Date.now()) {
    if (!["approve", "reject"].includes(decision)) throw new TypeError("审阅决定无效");
    const normalized = normalizeProposals(proposals);
    const target = normalized.find((proposal) => proposal.id === id);
    if (!target || target.status !== "pending_review") return null;
    if (decision === "approve" && target.benchmark.failed !== 0) return null;
    const status = decision === "approve" ? "approved_for_implementation" : "rejected";
    return normalized.map((proposal) => proposal.id === id
      ? { ...proposal, status, updatedAt: now, reviewedAt: now }
      : proposal);
  }

  return {
    CATEGORIES,
    MAX_PROPOSALS,
    STORAGE_VERSION,
    addProposal,
    createProposal,
    loadProposals,
    normalizeBenchmarkSnapshot,
    normalizeProposal,
    normalizeProposals,
    reviewProposal,
    serializeProposals,
  };
});
