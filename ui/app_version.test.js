const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { updateFooterVersion } = require("./app_version.js");

test("footer displays the version returned by the desktop app at runtime", async () => {
  const element = { textContent: "LocM" };
  const updated = await updateFooterVersion(element, async () => "0.2.11");

  assert.equal(updated, true);
  assert.equal(element.textContent, "LocM · v0.2.11");
});

test("footer version integration uses Tauri app.getVersion and contains no stale literal", () => {
  const root = __dirname;
  const html = fs.readFileSync(path.join(root, "index.html"), "utf8");
  const app = fs.readFileSync(path.join(root, "app.js"), "utf8");

  assert.match(html, /app_version\.js/);
  assert.doesNotMatch(html, /LocM\s*·\s*v0\.2\.9/);
  assert.match(app, /updateFooterVersion\(\$\("\.version"\),\s*\(\)\s*=>\s*tauri\.app\.getVersion\(\)\)/);
});
