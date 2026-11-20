(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.LocmAppVersion = api;
})(globalThis, function () {
  async function updateFooterVersion(element, getVersion) {
    if (!element || typeof getVersion !== "function") return false;
    try {
      const version = String(await getVersion() ?? "").trim().replace(/^v/i, "");
      if (!version) return false;
      element.textContent = `LocM · v${version}`;
      return true;
    } catch {
      return false;
    }
  }

  return { updateFooterVersion };
});
