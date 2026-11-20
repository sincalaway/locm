const test = require("node:test");
const assert = require("node:assert/strict");
const {
  addProposal,
  createProposal,
  loadProposals,
  reviewProposal,
  serializeProposals,
} = require("./agent_proposals.js");

const benchmark = {
  suiteId: "locm-agent-protocol-safety",
  suiteVersion: 1,
  total: 6,
  passed: 6,
  failed: 0,
  ranAt: 100,
  modelTested: true,
};

function proposal(overrides = {}) {
  return createProposal({
    id: "p-1",
    category: "tool_routing",
    observedBehavior: "模型把文件夹位置误当作新建目标。",
    proposedChange: "- old route\n+ new route",
    rationale: "使用固定回归用例验证路由变化。",
    taskId: "task-1",
    taskTitle: "路由回归",
    benchmark,
    ...overrides,
  }, 200);
}

test("creates a review-only proposal with sanitized code-only benchmark evidence", () => {
  const item = proposal();
  assert.equal(item.status, "pending_review");
  assert.equal(item.benchmark.modelTested, false);
  assert.equal(item.benchmark.scope, "protocol_safety_code_only");
  assert.equal(item.benchmark.passed, 6);
  assert.equal(Object.hasOwn(item, "apply"), false);
});

test("requires a baseline result and complete proposal details", () => {
  assert.throws(() => proposal({ benchmark: null }), /先运行并记录/);
  assert.throws(() => proposal({ benchmark: { ...benchmark, suiteId: "unverified" } }), /先运行并记录/);
  assert.throws(() => proposal({ rationale: "  " }), /都不能为空/);
  assert.throws(() => proposal({ category: "binary" }), /类别无效/);
});

test("approval marks a proposal for implementation but does not apply it", () => {
  const item = proposal();
  const reviewed = reviewProposal([item], item.id, "approve", 300);
  assert.equal(reviewed[0].status, "approved_for_implementation");
  assert.equal(reviewed[0].reviewedAt, 300);
  assert.equal(Object.hasOwn(reviewed[0], "appliedAt"), false);
  assert.equal(reviewProposal(reviewed, item.id, "reject", 301), null);
});

test("does not approve proposals when their recorded baseline suite has failures", () => {
  const item = proposal({ benchmark: { ...benchmark, passed: 5, failed: 1, total: 6 } });
  assert.equal(reviewProposal([item], item.id, "approve", 300), null);
  const rejected = reviewProposal([item], item.id, "reject", 300);
  assert.equal(rejected[0].status, "rejected");
});

test("rejects malformed, unknown-version, duplicate, and oversized stored records safely", () => {
  const item = proposal();
  assert.deepEqual(loadProposals("not-json"), []);
  assert.deepEqual(loadProposals({ version: 99, proposals: [item] }), []);
  const stored = serializeProposals([item, { ...item, id: "p-2", proposedChange: "x".repeat(9000) }]);
  const loaded = loadProposals(stored);
  assert.equal(loaded.length, 2);
  assert.ok(loaded.every((entry) => entry.proposedChange.length <= 8000));
  assert.equal(loadProposals(serializeProposals([item, { ...item }])).length, 1);
  const large = proposal({
    id: "large",
    observedBehavior: "a".repeat(4000),
    proposedChange: "x".repeat(8000),
    rationale: "y".repeat(4000),
  });
  const bounded = serializeProposals(Array.from({ length: 30 }, (_, index) => ({ ...large, id: `large-${index}` })));
  assert.ok(Buffer.byteLength(bounded, "utf8") <= 512 * 1024);
});

test("caps proposal history and refuses a second ID replacement to duplicate an item", () => {
  let items = [];
  for (let index = 0; index < 35; index++) {
    items = addProposal(items, proposal({ id: `p-${index}` }));
  }
  assert.equal(items.length, 30);
  assert.equal(new Set(items.map((item) => item.id)).size, 30);
});
