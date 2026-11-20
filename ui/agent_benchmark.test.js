const test = require("node:test");
const assert = require("node:assert/strict");
const { runSuite, SUITE_ID, SUITE_VERSION } = require("./agent_benchmark.js");

test("offline benchmark covers protocol and safety invariants without claiming model execution", () => {
  const result = runSuite(123);
  assert.equal(result.suiteId, SUITE_ID);
  assert.equal(result.suiteVersion, SUITE_VERSION);
  assert.equal(result.total, 6);
  assert.equal(result.passed, 6);
  assert.equal(result.failed, 0);
  assert.equal(result.passRate, 1);
  assert.equal(result.safetyChecks, 3);
  assert.equal(result.safetyChecksFailed, 0);
  assert.equal(result.modelTested, false);
  assert.match(result.scope, /未启动模型/);
  assert.ok(result.results.every((item) => item.passed));
});

test("fixed benchmark results repeat exactly for the same recorded time", () => {
  assert.deepEqual(runSuite(456), runSuite(456));
});

test("benchmark fixtures are descriptive and never report a live end-to-end run", () => {
  const result = runSuite(789);
  assert.deepEqual(result.results.map((item) => item.id), [
    "valid-plan",
    "prose-rejected",
    "disabled-write-blocked",
    "unknown-tool-blocked",
    "duplicate-plan-paused",
    "interrupted-side-effect-blocked",
  ]);
  assert.ok(result.results.every((item) => typeof item.detail === "string" && item.detail.length > 0));
  assert.equal(result.modelTested, false);
});
