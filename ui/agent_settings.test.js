const test = require("node:test");
const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

const html = readFileSync(join(__dirname, "index.html"), "utf8");

test("workspace tool settings collapse by default while the master switch stays visible", () => {
  const detailsStart = html.indexOf('<details id="agent-tool-settings" class="agent-tool-settings">');
  assert.notEqual(detailsStart, -1);
  const openingEnd = html.indexOf(">", detailsStart);
  const detailsEnd = html.indexOf("</details>", openingEnd);
  assert.ok(openingEnd > detailsStart);
  assert.ok(detailsEnd > openingEnd);
  assert.doesNotMatch(html.slice(detailsStart, openingEnd), /\sopen(?:\s|>)/);
  assert.match(html.slice(detailsStart, detailsEnd), /<summary>/);

  const masterSwitch = html.indexOf('id="agent-tool-all"', detailsEnd);
  assert.ok(masterSwitch > detailsEnd, "master switch should remain outside the collapsible details");
  const panelEnd = html.indexOf("</div>", masterSwitch);
  assert.ok(panelEnd > masterSwitch);
  assert.match(html.slice(masterSwitch, panelEnd), /aria-controls="agent-tool-switches"/);
});
