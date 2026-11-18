(() => {
  const tauri = window.__TAURI__;
  const invoke = (command, args = {}) => {
    if (!tauri?.core?.invoke) return Promise.reject(new Error("请从桌面应用窗口启动本地 AI 工作台。"));
    return tauri.core.invoke(command, args);
  };
  const listen = tauri?.event?.listen;
  const CHAT_STORAGE_KEY = "locm-local-chat-sessions-v1";
  const CURRENT_CHAT_KEY = "locm-current-chat-session-v1";
  const CHAT_PREFERENCES_KEY = "locm-chat-preferences-v1";
  const RUN_PREFERENCES_KEY = "locm-run-preferences-v1";
  const state = {
    settings: null,
    hardware: null,
    engines: new Map(),
    mediaInstallSupported: false,
    mediaInstallBusy: false,
    models: [],
    pendingRunModel: null,
    runtime: { running: false, port: 8686, modelName: null },
    runCapabilities: null,
    runCapabilitiesLoading: null,
    runPreferences: loadRunPreferences(),
    samples: [],
    sessions: loadSessions(),
    currentSessionId: loadCurrentSessionId(),
    busyChat: false,
    busySessionId: null,
    draftAttachments: [],
    chatPreferences: loadChatPreferences(),
    chatPersistenceAvailable: false,
    chatSaveQueue: Promise.resolve(),
    toastTimer: null,
  };
  const metricBuffer = [];
  const fmtBytes = (bytes) => {
    if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
    const units = ["B", "KB", "MB", "GB", "TB"];
    const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
    return `${(bytes / 1024 ** index).toFixed(index ? 1 : 0)} ${units[index]}`;
  };
  const escapeText = (value) => String(value ?? "").replace(/[&<>"']/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "\"": "&quot;", "\'": "&#39;" }[character]));
  const $ = (selector, root = document) => root.querySelector(selector);
  const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];

  function showToast(message, type = "success") {
    const box = $("#toast");
    box.textContent = message;
    box.className = `toast ${type}`;
    box.hidden = false;
    clearTimeout(state.toastTimer);
    state.toastTimer = setTimeout(() => { box.hidden = true; }, 3800);
  }

  function setPage(page) {
    const safePage = document.getElementById(`page-${page}`) ? page : "overview";
    $$(".page").forEach((view) => view.classList.toggle("active", view.id === `page-${safePage}`));
    $$(".nav-item").forEach((item) => {
      const active = item.dataset.page === safePage;
      item.classList.toggle("active", active);
      if (active) item.setAttribute("aria-current", "page"); else item.removeAttribute("aria-current");
    });
    $("#page-title").textContent = $( `#page-${safePage}` ).dataset.title || safePage;
    if (safePage === "models") loadModels();
    if (safePage === "run") { loadEngines(); refreshRuntime(); loadModels(); loadRunCapabilities(true); }
    if (safePage === "image" || safePage === "video") renderMediaEngineBanners();
    if (safePage === "chat") { renderChat(); refreshRuntime(); }
    if (safePage === "monitor") { drawChart(); refreshSystem(); }
    if (safePage === "settings") loadSettings();
  }

  function loadSessions() {
    try {
      const value = JSON.parse(localStorage.getItem(CHAT_STORAGE_KEY) || "[]");
      return Array.isArray(value) ? value.filter((entry) => entry && Array.isArray(entry.messages)) : [];
    } catch { return []; }
  }
  function loadCurrentSessionId() {
    try { return localStorage.getItem(CURRENT_CHAT_KEY); } catch { return null; }
  }
  function loadChatPreferences() {
    const defaults = { temperature: 0.7, includeHistory: true };
    try {
      const saved = JSON.parse(localStorage.getItem(CHAT_PREFERENCES_KEY) || "{}");
      const temperature = Number(saved.temperature);
      return {
        temperature: Number.isFinite(temperature) ? Math.max(0, Math.min(1.5, temperature)) : defaults.temperature,
        includeHistory: saved.includeHistory !== false,
      };
    } catch { return defaults; }
  }
  function saveChatPreferences() {
    try { localStorage.setItem(CHAT_PREFERENCES_KEY, JSON.stringify(state.chatPreferences)); }
    catch { showToast("对话设置无法保存到本机浏览器存储", "error"); }
  }
  async function loadChatHistory() {
    const legacy = state.sessions;
    try {
      const stored = await invoke("load_chat_sessions");
      state.chatPersistenceAvailable = true;
      state.sessions = Array.isArray(stored) && stored.length ? stored : legacy;
      if ((!stored || !stored.length) && legacy.length) saveSessions();
      const requested = loadCurrentSessionId();
      state.currentSessionId = state.sessions.some((session) => session.id === requested)
        ? requested : (state.sessions[0]?.id || null);
    } catch (error) {
      state.chatPersistenceAvailable = false;
      state.sessions = legacy;
      state.currentSessionId = state.sessions.some((session) => session.id === state.currentSessionId)
        ? state.currentSessionId : (state.sessions[0]?.id || null);
      showToast(`本机聊天文件不可用，将使用本地浏览器存储：${error}`, "error");
    }
  }
  function saveSessions() {
    state.sessions = state.sessions.slice(0, 100);
    try { localStorage.setItem(CURRENT_CHAT_KEY, state.currentSessionId || ""); } catch { /* The native store remains primary. */ }
    if (!state.chatPersistenceAvailable) {
      try { localStorage.setItem(CHAT_STORAGE_KEY, JSON.stringify(state.sessions)); }
      catch { showToast("对话无法保存到本机存储空间", "error"); }
      return;
    }
    const snapshot = JSON.parse(JSON.stringify(state.sessions));
    state.chatSaveQueue = state.chatSaveQueue.catch(() => {}).then(() => invoke("save_chat_sessions", { sessions: snapshot })).catch((error) => {
      state.chatPersistenceAvailable = false;
      try { localStorage.setItem(CHAT_STORAGE_KEY, JSON.stringify(snapshot)); } catch { /* Report the primary failure below. */ }
      showToast(`写入本机聊天历史失败：${error}`, "error");
    });
  }
  function currentSession() {
    return state.sessions.find((session) => session.id === state.currentSessionId) || null;
  }
  function createSession() {
    const session = { id: `${Date.now()}-${Math.random().toString(16).slice(2)}`, title: "新对话", updatedAt: Date.now(), messages: [] };
    state.sessions.unshift(session);
    state.currentSessionId = session.id;
    saveSessions();
    renderChat();
    $("#chat-input").focus();
  }
  function ensureSession() {
    let session = currentSession();
    if (!session) {
      if (state.sessions.length) {
        session = state.sessions[0];
        state.currentSessionId = session.id;
      } else {
        createSession();
        session = currentSession();
      }
    }
    return session;
  }

  function loadRunPreferences() {
    const defaults = { context: "auto", flashAttention: "auto", kvCacheQ8: false, lanAccess: false, autoRestart: false };
    try {
      const saved = JSON.parse(localStorage.getItem("locm-run-preferences-v1") || "{}");
      return {
        context: ["auto", "8192", "16384", "32768", "65536"].includes(saved.context) ? saved.context : defaults.context,
        flashAttention: ["auto", "on", "off"].includes(saved.flashAttention) ? saved.flashAttention : defaults.flashAttention,
        kvCacheQ8: saved.kvCacheQ8 === true,
        lanAccess: saved.lanAccess === true,
        autoRestart: saved.autoRestart === true,
      };
    } catch { return defaults; }
  }
  function saveRunPreferences() {
    try { localStorage.setItem(RUN_PREFERENCES_KEY, JSON.stringify(state.runPreferences)); }
    catch { showToast("运行偏好无法保存到本机浏览器存储", "error"); }
  }
  function renderLanAccessNote() {
    $("#lan-access-note").textContent = state.runPreferences.lanAccess
      ? "已开启：启动时只绑定私有 IPv4 地址；同一私有网段设备可访问未认证 API。仅在可信网络使用。"
      : "默认关闭、仅绑定 127.0.0.1。开启后同一私有网段设备可访问未认证 API；只在可信网络开启，且仅绑定一个私有 IPv4 地址。";
  }
  function renderRunPreferences() {
    $$('input[name="run-context"]').forEach((input) => { input.checked = input.value === state.runPreferences.context; });
    $("#flash-attention").value = state.runPreferences.flashAttention;
    $("#kv-cache-q8").checked = state.runPreferences.kvCacheQ8;
    $("#auto-restart").checked = state.runPreferences.autoRestart;
    $("#lan-access").checked = state.runPreferences.lanAccess;
    renderLanAccessNote();
    $("#context-current-label").textContent = `· ${state.runPreferences.context === "auto" ? "自动" : `${Number(state.runPreferences.context) / 1024}K`}`;
    renderRunCapabilityStatus();
    updateRunEstimates();
  }
  function updateRunPreference(key, value) {
    state.runPreferences[key] = value;
    saveRunPreferences();
    renderRunPreferences();
  }
  function effectiveRunOptions() {
    const capabilities = state.runCapabilities || {};
    return {
      context: capabilities.ctxSize ? state.runPreferences.context : "auto",
      flashAttention: capabilities.flashAttention ? state.runPreferences.flashAttention : "auto",
      kvCacheQ8: Boolean(capabilities.cacheTypeK && capabilities.cacheTypeV && state.runPreferences.kvCacheQ8),
      lanAccess: state.runPreferences.lanAccess,
      autoRestart: state.runPreferences.autoRestart,
    };
  }

  async function loadRunCapabilities(force = false) {
    if (force) state.runCapabilities = null;
    if (state.runCapabilities) return;
    if (state.runCapabilitiesLoading) return state.runCapabilitiesLoading;
    state.runCapabilitiesLoading = (async () => {
      try {
        state.runCapabilities = await invoke("server_capabilities");
        renderRunPreferences();
      } catch (error) {
        state.runCapabilities = { host: false, port: false, ctxSize: false, flashAttention: false, cacheTypeK: false, cacheTypeV: false, nGpuLayers: false };
        renderRunCapabilityStatus();
        $("#run-capability-summary").textContent = `未检测到可用的已安装 llama-server：${error}`;
        refreshRuntime();
      }
    })();
    try { await state.runCapabilitiesLoading; }
    finally { state.runCapabilitiesLoading = null; }
  }
  function renderRunCapabilityStatus() {
    const capabilities = state.runCapabilities;
    if (capabilities) {
      let changed = false;
      if (!capabilities.ctxSize && state.runPreferences.context !== "auto") { state.runPreferences.context = "auto"; changed = true; }
      if (!capabilities.flashAttention && state.runPreferences.flashAttention !== "auto") { state.runPreferences.flashAttention = "auto"; changed = true; }
      if (!(capabilities.cacheTypeK && capabilities.cacheTypeV) && state.runPreferences.kvCacheQ8) { state.runPreferences.kvCacheQ8 = false; changed = true; }
      if (!(capabilities.host && capabilities.port) && state.runPreferences.lanAccess) { state.runPreferences.lanAccess = false; changed = true; }
      if (changed) saveRunPreferences();
      $$('input[name="run-context"]').forEach((input) => { input.checked = input.value === state.runPreferences.context; });
      $("#flash-attention").value = state.runPreferences.flashAttention;
      $("#kv-cache-q8").checked = state.runPreferences.kvCacheQ8;
      $("#lan-access").checked = state.runPreferences.lanAccess;
      renderLanAccessNote();
      $("#context-current-label").textContent = `· ${state.runPreferences.context === "auto" ? "自动" : `${Number(state.runPreferences.context) / 1024}K`}`;
    }
    const supported = [];
    if (capabilities?.host && capabilities?.port) supported.push("--host", "--port");
    if (capabilities?.ctxSize) supported.push("--ctx-size");
    if (capabilities?.flashAttention) supported.push("--flash-attn");
    if (capabilities?.cacheTypeK && capabilities?.cacheTypeV) supported.push("--cache-type-k/v");
    const summary = capabilities
      ? `已检测服务器参数：${supported.length ? supported.join("、") : "未发现可选加速参数"}。`
      : "正在检查已安装 llama-server 的参数…";
    $("#run-capability-summary").textContent = summary;
    const contextSupported = Boolean(capabilities?.ctxSize);
    $$('input[name="run-context"]').forEach((input) => { input.disabled = input.value !== "auto" && !contextSupported; });
    $("#flash-attention").disabled = !capabilities?.flashAttention;
    $("#flash-capability-note").textContent = capabilities?.flashAttention
      ? "启动时按所选值传入当前版本支持的 --flash-attn"
      : "当前版本未确认支持 --flash-attn，选项已禁用";
    const kvSupported = Boolean(capabilities?.cacheTypeK && capabilities?.cacheTypeV);
    $("#kv-cache-q8").disabled = !kvSupported;
    $("#kv-capability-note").textContent = kvSupported
      ? "同时为 K/V 缓存传入 q8_0；可能降低内存占用并影响速度"
      : "未同时确认支持 K/V 缓存参数，选项已禁用";
    $("#lan-access").disabled = !capabilities?.host || !capabilities?.port;
    const effective = effectiveRunOptions();
    const active = [];
    if (effective.context !== "auto") active.push(`上下文 ${Number(effective.context) / 1024}K`);
    else active.push("自动上下文（不传 --ctx-size）");
    active.push(`Flash Attention ${effective.flashAttention}`);
    if (effective.kvCacheQ8) active.push("K/V 缓存 q8_0");
    active.push(effective.lanAccess ? "私有 IPv4 局域网绑定" : "仅 127.0.0.1");
    if (effective.autoRestart) active.push("崩溃后最多自动重启一次");
    $("#run-effective-options").textContent = `当前设置：${active.join("；")}。思考行为不强制覆盖模型模板。`;
    $("#start-model").disabled = Boolean(state.runtime.running) || !$("#run-model-select").value || !capabilities?.host || !capabilities?.port;
  }
  function updateRunEstimates() {
    const model = state.models.find((item) => item.name === $("#run-model-select")?.value);
    if (!model) {
      $("#run-plan-summary").textContent = "选择一个已安装模型后查看本机资源估算。";
      $("#run-gpu-value").textContent = "—";
      $("#run-ram-value").textContent = "—";
      $("#run-disk-value").textContent = "—";
      $("#estimated-speed").textContent = "—";
      $("#estimated-load").textContent = "—";
      $("#run-gpu-bar").style.width = "0%";
      $("#run-ram-bar").style.width = "0%";
      $("#run-disk-bar").style.width = "0%";
      return;
    }
    const sizeGiB = Math.max(model.sizeBytes / (1024 ** 3), 0.05);
    const sizeMiB = model.sizeBytes / (1024 ** 2);
    const hardware = state.hardware || {};
    const availableGiB = (hardware.availableMemoryBytes || 0) / (1024 ** 3);
    const totalGiB = (hardware.totalMemoryBytes || 0) / (1024 ** 3);
    const cores = Math.max(1, hardware.logicalCores || 4);
    const estimatedSpeed = Math.max(0.5, Math.min(80, 10.1 / Math.sqrt(sizeGiB) * Math.sqrt(cores / 8)));
    const estimatedLoadSeconds = Math.max(1, Math.ceil(sizeMiB / 120));
    const memoryNeedGiB = sizeGiB + 0.4;
    const ramPercent = totalGiB ? Math.min(100, memoryNeedGiB / totalGiB * 100) : 0;
    const diskPercent = totalGiB ? Math.min(100, sizeGiB / totalGiB * 100) : 0;
    $("#run-plan-summary").textContent = availableGiB >= memoryNeedGiB
      ? `${model.name} · 约 ${sizeGiB.toFixed(1)} GB 模型文件；现有可用内存预计足以容纳模型与运行开销。`
      : `${model.name} · 约 ${sizeGiB.toFixed(1)} GB 模型文件；预计会使用内存映射，可能较慢或受系统可用内存影响。`;
    $("#run-gpu-value").textContent = hardware.gpuName ? "不使用 GPU 层" : "0 GB · CPU";
    $("#run-gpu-note").textContent = hardware.gpuName
      ? `${hardware.gpuName} 已检测，但当前安装为 CPU 路径，不会使用显存。`
      : "未检测到独立显卡；当前 Windows x64 引擎安装器提供 CPU 路径。";
    $("#run-ram-value").textContent = `${availableGiB.toFixed(1)} GB 可用 / ${totalGiB.toFixed(1)} GB · 模型约 ${sizeGiB.toFixed(1)} GB`;
    $("#run-ram-note").textContent = `估算模型加运行开销约 ${memoryNeedGiB.toFixed(1)} GB；实际占用取决于上下文与缓存设置。`;
    $("#run-disk-value").textContent = `${sizeGiB.toFixed(1)} GB · 按需读取`;
    $("#run-gpu-bar").style.width = hardware.gpuName ? "0%" : "0%";
    $("#run-ram-bar").style.width = `${ramPercent}%`;
    $("#run-disk-bar").style.width = `${diskPercent}%`;
    $("#estimated-speed").textContent = `约 ${estimatedSpeed.toFixed(0)} tok/s`;
    $("#estimated-load").textContent = `约 ${estimatedLoadSeconds} 秒`;
  }

  async function loadHardware() {
    try {
      state.hardware = await invoke("hardware_snapshot");
      const hw = state.hardware;
      const ram = fmtBytes(hw.totalMemoryBytes);
      const free = fmtBytes(hw.availableMemoryBytes);
      $("#hardware-cpu").textContent = hw.cpuName || "未识别处理器";
      $("#hardware-cores").textContent = `${hw.logicalCores || 0} 个逻辑处理器 · ${hw.os || ""} ${hw.architecture || ""}`;
      $("#hardware-memory").textContent = ram;
      $("#memory-free").textContent = `可用 ${free}`;
      $("#hardware-gpu").textContent = hw.gpuName || "未检测到独立显卡";
      $("#gpu-memory").textContent = hw.gpuMemoryBytes ? `显存 ${fmtBytes(hw.gpuMemoryBytes)}` : "当前以 CPU 推理为主";
      const availableGb = hw.availableMemoryBytes / (1024 ** 3);
      $("#overview-headline").textContent = hw.gpuName ? `${hw.gpuName} · ${ram} 内存` : `${ram} 内存 · 可使用 CPU 推理`;
      $("#overview-copy").textContent = availableGb < 2.5
        ? `当前可用内存约 ${availableGb.toFixed(1)} GB，建议优先选择小体积量化 GGUF；本机尚未检测到受支持的 NVIDIA 显卡。`
        : `已检测 ${hw.cpuName}，约 ${availableGb.toFixed(1)} GB 内存可用。模型是否能运行取决于文件大小、量化和系统资源。`;
      $("#monitor-cpu-name").textContent = `${hw.cpuName} · ${hw.logicalCores} 个逻辑处理器`;
      updateRunEstimates();
    } catch (error) {
      $("#overview-headline").textContent = "无法检测本机配置";
      $("#overview-copy").textContent = String(error);
      showToast(`硬件检测失败：${error}`, "error");
    }
  }

  function engineCardMarkup(engine, status) {
    const chat = engine.engine === "llama.cpp";
    const installed = status?.installedVersions || [];
    const purpose = chat ? "对话模型运行时" : "图像与视频共用运行时";
    const extra = chat ? "聊天和 GGUF 模型使用" : "画图和视频共用，只需管理一份";
    const id = engine.engine;
    return `<article class="engine-card" data-engine="${id}">
      <div class="engine-card-top"><div><div class="engine-kicker">${purpose}</div><h3>${id}</h3></div><span class="status-pill ${installed.length ? "ready" : "empty"}">${installed.length ? `${installed.length} 个版本已安装` : "尚未安装"}</span></div>
      <p class="engine-description">${extra}。运行时来自官方 GitHub Releases 并在安装前校验 SHA-256。</p>
      <div class="engine-meta"><div class="metadata-row"><span>已安装版本</span><strong>${installed.length ? installed.map((version) => escapeText(version.tag)).join("、") : "暂无"}</strong></div><div class="metadata-row path-row"><span>目录</span><code title="${escapeText(status?.installPath || "")}">${escapeText(status?.installPath || "读取中…")}</code></div></div>
      <div class="release-box" data-release hidden></div><div class="engine-progress" data-progress hidden></div><div class="engine-error" data-error hidden></div>
      <div class="engine-actions"><button class="button secondary" data-engine-action="check">检查官方版本</button><button class="button primary" data-engine-action="install" disabled>先检查版本</button></div>
    </article>`;
  }

  async function loadEngines() {
    try {
      const statuses = await invoke("engine_statuses");
      state.engines = new Map(statuses.map((status) => [status.engine, status]));
      state.mediaInstallSupported = await invoke("engine_install_supported");
      const supported = state.mediaInstallSupported;
      const engines = statuses.map((status) => ({ engine: status.engine }));
      $("#engine-cards").innerHTML = engines.map((engine) => engineCardMarkup(engine, state.engines.get(engine.engine))).join("");
      $("#overview-engines").innerHTML = engines.map((engine) => engineCardMarkup(engine, state.engines.get(engine.engine))).join("");
      [$("#engine-cards"), $("#overview-engines")].forEach((container) => {
        $$(".engine-card", container).forEach((card) => wireEngineCard(card, supported));
      });
      if (!supported) {
        $$(".engine-card").forEach((card) => {
          const button = $("[data-engine-action=install]", card);
          button.disabled = true;
          button.textContent = "仅支持 Windows x64 安装";
        });
      }
      renderMediaEngineBanners();
    } catch (error) {
      [$("#engine-cards"), $("#overview-engines")].forEach((box) => { box.innerHTML = `<div class="error-box">无法读取引擎状态：${escapeText(error)}</div>`; });
      renderMediaEngineBanners(String(error));
    }
  }

  function renderMediaEngineBanners(loadError = "") {
    const engineId = "stable-diffusion.cpp";
    const known = state.engines.has(engineId);
    const versions = state.engines.get(engineId)?.installedVersions || [];
    const installed = versions.length > 0;
    ["image", "video"].forEach((page) => {
      const status = $(`#${page}-engine-status`);
      const description = $(`#${page}-engine-description`);
      const install = $(`#${page}-install-engine`);
      const manage = $(`#${page}-manage-engine`);
      const error = $(`#${page}-engine-error`);
      if (!status || !description || !install || !manage || !error) return;
      if (loadError) { error.textContent = `无法读取共享引擎状态：${loadError}`; error.hidden = false; }
      else if (known) error.hidden = true;
      if (installed) {
        status.className = "status-pill ready";
        status.textContent = `已安装 · ${versions.map((version) => version.tag).join("、")}`;
        description.textContent = "画图与视频共用的 stable-diffusion.cpp 已安装。模型权重仍需单独管理；当前版本尚未接入图像/视频生成流程。";
        install.hidden = true;
        manage.hidden = false;
      } else {
        status.className = "status-pill empty";
        status.textContent = known ? "尚未安装" : "状态未知";
        description.textContent = "画图和视频共用 stable-diffusion.cpp，安装一次即可。引擎安装不会下载模型权重，也不代表生成能力已接入。";
        install.hidden = false;
        manage.hidden = true;
        install.disabled = !known || !state.mediaInstallSupported || state.mediaInstallBusy;
        install.textContent = !known ? "无法读取引擎状态" : (!state.mediaInstallSupported ? "仅支持 Windows x64 安装" : (state.mediaInstallBusy ? "正在安装…" : "一键安装"));
      }
    });
  }

  async function installMediaEngine() {
    if (state.mediaInstallBusy) return;
    if (!state.mediaInstallSupported) { showToast("创作引擎安装器仅支持 Windows x64", "error"); return; }
    state.mediaInstallBusy = true;
    let failure = "";
    ["image", "video"].forEach((page) => {
      const progress = $(`#${page}-engine-progress`);
      if (progress) { progress.hidden = false; progress.textContent = "正在查询 stable-diffusion.cpp 官方版本…"; }
      const error = $(`#${page}-engine-error`);
      if (error) error.hidden = true;
    });
    renderMediaEngineBanners();
    try {
      const latest = await invoke("engine_latest", { engine: "media" });
      const installedVersions = state.engines.get("stable-diffusion.cpp")?.installedVersions || [];
      if (installedVersions.some((version) => version.tag === latest.tag)) {
        showToast(`stable-diffusion.cpp ${latest.tag} 已安装；可在“运行”页管理版本`);
        return;
      }
      if (latest.prerelease && !window.confirm("官方最新版本是预览版。是否继续安装？")) return;
      ["image", "video"].forEach((page) => {
        const progress = $(`#${page}-engine-progress`);
        if (progress) progress.textContent = `正在安装 ${latest.tag}…`;
      });
      const statuses = await invoke("engine_install", { engine: "media", allowPrerelease: Boolean(latest.prerelease) });
      state.engines = new Map(statuses.map((item) => [item.engine, item]));
      showToast(`stable-diffusion.cpp ${latest.tag} 已安装；引擎尚未启动`);
      await loadEngines();
    } catch (error) {
      failure = String(error);
      showToast(`创作引擎安装失败：${error}`, "error");
    } finally {
      state.mediaInstallBusy = false;
      renderMediaEngineBanners(failure);
    }
  }

  function wireEngineCard(card, installSupported) {
    const engine = card.dataset.engine === "llama.cpp" ? "chat" : "media";
    const check = $("[data-engine-action=check]", card);
    const install = $("[data-engine-action=install]", card);
    const releaseBox = $("[data-release]", card);
    const errorBox = $("[data-error]", card);
    let latest = null;
    check.addEventListener("click", async () => {
      check.disabled = true; check.textContent = "正在查询…"; errorBox.hidden = true;
      try {
        latest = await invoke("engine_latest", { engine });
        releaseBox.hidden = false;
        releaseBox.replaceChildren();
        const version = document.createElement("strong");
        version.textContent = `${latest.tag}${latest.prerelease ? " · 官方预览版" : ""}`;
        const detail = document.createElement("small");
        detail.textContent = `${fmtBytes(latest.sizeBytes)} · ${latest.assetName}`;
        const link = document.createElement("a");
        link.href = latest.releaseUrl; link.target = "_blank"; link.rel = "noreferrer"; link.textContent = "查看官方 Release";
        releaseBox.append(version, detail, link);
        const already = (state.engines.get(card.dataset.engine)?.installedVersions || []).some((item) => item.tag === latest.tag);
        install.disabled = !installSupported || already;
        install.textContent = already ? "当前版本已安装" : (engine === "chat" && latest.prerelease ? "安装此预览版" : "安装 / 更新");
      } catch (error) { errorBox.textContent = `版本查询失败：${error}`; errorBox.hidden = false; }
      finally { check.disabled = false; check.textContent = "检查官方版本"; }
    });
    install.addEventListener("click", async () => {
      if (!latest || !installSupported) return;
      if (latest.prerelease && !window.confirm("此为官方预览版。是否继续安装？")) return;
      install.disabled = true; install.textContent = "正在安装…"; errorBox.hidden = true;
      const progress = $("[data-progress]", card); progress.hidden = false; progress.textContent = "正在下载官方压缩包…";
      try {
        const statuses = await invoke("engine_install", { engine, allowPrerelease: Boolean(latest.prerelease) });
        state.engines = new Map(statuses.map((item) => [item.engine, item]));
        progress.textContent = "安装并 SHA-256 校验完成；引擎尚未启动。";
        showToast(`${latest.engine} ${latest.tag} 已安装`);
        await loadEngines();
        if (engine === "chat") await loadRunCapabilities(true);
      } catch (error) { errorBox.textContent = `安装失败：${error}`; errorBox.hidden = false; install.disabled = false; install.textContent = "重试安装"; }
    });
  }

  async function loadModels() {
    try {
      state.models = await invoke("list_models");
      $("#model-count").textContent = state.models.length;
      const list = $("#models-installed");
      if (!state.models.length) {
        list.innerHTML = `<div class="empty-state"><strong>还没有 GGUF 模型</strong><p>从电脑导入一个 GGUF 文件，或搜索 Hugging Face 模型。</p><button class="button secondary" id="empty-import">选择本地文件</button></div>`;
        $("#empty-import").addEventListener("click", importLocalModel);
      } else {
        list.replaceChildren();
        state.models.forEach((model) => {
          const row = document.createElement("article"); row.className = "model-row model-card";
          const info = document.createElement("div"); info.className = "model-row-info";
          const title = document.createElement("strong"); title.textContent = model.name;
          title.title = model.name;
          const path = document.createElement("code"); path.textContent = model.path; path.title = model.path;
          const type = document.createElement("small"); type.className = "model-type"; type.textContent = "GGUF · 本地模型";
          info.append(title, type, path);
          const size = document.createElement("div"); size.className = "model-size-block";
          const sizeValue = document.createElement("strong"); sizeValue.textContent = fmtBytes(model.sizeBytes);
          const sizeLabel = document.createElement("small"); sizeLabel.textContent = "文件大小";
          size.append(sizeValue, sizeLabel);
          const actions = document.createElement("div"); actions.className = "model-actions";
          const open = document.createElement("button"); open.className = "button secondary"; open.textContent = "打开位置";
          open.addEventListener("click", async () => {
            open.disabled = true;
            try { await invoke("open_model_location", { modelName: model.name }); }
            catch (error) { showToast(`打开模型位置失败：${error}`, "error"); }
            finally { open.disabled = false; }
          });
          const remove = document.createElement("button"); remove.className = "button danger"; remove.textContent = "移除";
          remove.addEventListener("click", async () => {
            if (!window.confirm(`确定从模型目录删除“${model.name}”？此操作不可撤销。`)) return;
            remove.disabled = true;
            try {
              state.models = await invoke("remove_model", { modelName: model.name });
              await loadModels(); showToast("模型文件已从本机移除");
            } catch (error) { showToast(`移除失败：${error}`, "error"); remove.disabled = false; }
          });
          const run = document.createElement("button"); run.className = "button primary"; run.textContent = "运行";
          run.addEventListener("click", () => { state.pendingRunModel = model.name; location.hash = "#run"; refreshModelSelect(); });
          actions.append(open, remove, run);
          row.append(info, size, actions); list.append(row);
        });
      }
      refreshModelSelect();
    } catch (error) { $("#models-installed").innerHTML = `<div class="error-box">无法读取模型列表：${escapeText(error)}</div>`; }
  }
  function refreshModelSelect() {
    const select = $("#run-model-select");
    const selected = state.pendingRunModel || select.value;
    select.replaceChildren(new Option("选择已安装模型…", ""));
    state.models.forEach((model) => select.add(new Option(`${model.name} · ${fmtBytes(model.sizeBytes)}`, model.name)));
    if (state.models.some((model) => model.name === selected)) select.value = selected;
    state.pendingRunModel = null;
    refreshChatModelSelect();
    updateRunEstimates();
    renderRunCapabilityStatus();
  }

  function refreshChatModelSelect() {
    const select = $("#chat-model-select");
    if (!select) return;
    const selected = state.runtime.running ? state.runtime.modelName : select.value;
    select.replaceChildren(new Option("本地模型：选择并启动…", ""));
    state.models.forEach((model) => select.add(new Option(`${model.name} · ${fmtBytes(model.sizeBytes)}`, model.name)));
    if (state.models.some((model) => model.name === selected)) select.value = selected;
    select.disabled = !state.models.length || state.busyChat;
  }

  function updateChatSendState() {
    const input = $("#chat-input");
    const send = $("#send-chat");
    if (!input || !send) return;
    send.disabled = !state.runtime.running || state.busyChat || (!input.value.trim() && !state.draftAttachments.length);
  }

  async function importLocalModel() {
    try {
      const source = await invoke("choose_model_file");
      if (!source) return;
      $("#pick-model").disabled = true; $("#pick-model").textContent = "正在导入…";
      await invoke("import_model", { source });
      await loadModels();
      showToast("GGUF 模型已导入本机模型目录");
    } catch (error) { showToast(`导入失败：${error}`, "error"); }
    finally { $("#pick-model").disabled = false; $("#pick-model").textContent = "导入本地 GGUF"; }
  }

  function renderRecommendedModels() {
    const recommendations = [
      { name: "轻量入门", size: "约 1–4B 参数", query: "Qwen3 1.7B GGUF", detail: "适合内存较紧张的设备；优先查看 Q4 量化文件。" },
      { name: "均衡通用", size: "约 4–8B 参数", query: "Qwen3 4B GGUF", detail: "适合日常问答与写作；实际内存需求取决于量化和上下文。" },
      { name: "较强能力", size: "约 8–14B 参数", query: "Qwen3 14B GGUF", detail: "模型文件和运行内存更大；下载前先核对文件大小。" },
    ];
    const box = $("#models-recommended");
    box.replaceChildren();
    const note = document.createElement("p");
    note.className = "muted small recommendation-note";
    note.textContent = "按参数规模浏览建议；模型是否兼容及下载文件大小，以 Hugging Face 仓库信息为准。";
    box.append(note);
    recommendations.forEach((item) => {
      const card = document.createElement("article"); card.className = "recommend-card card";
      const badge = document.createElement("span"); badge.className = "recommend-badge"; badge.textContent = item.size;
      const title = document.createElement("h2"); title.textContent = item.name;
      const description = document.createElement("p"); description.textContent = item.detail;
      const button = document.createElement("button"); button.className = "button secondary"; button.textContent = "在 Hugging Face 查找";
      button.addEventListener("click", () => {
        $("#model-search-query").value = item.query;
        switchModelTab("download");
        $("#model-search-form").requestSubmit();
      });
      card.append(badge, title, description, button); box.append(card);
    });
  }

  function renderHfModels(models) {
    const box = $("#hf-results"); box.replaceChildren();
    if (!models.length) { box.innerHTML = `<div class="empty-state">没有找到模型。换一个关键词试试。</div>`; return; }
    models.forEach((model) => {
      const card = document.createElement("article"); card.className = "hf-model-card";
      const details = document.createElement("div");
      const repo = document.createElement("strong"); repo.textContent = model.repoId;
      const meta = document.createElement("small"); meta.textContent = `${Number(model.downloads || 0).toLocaleString()} 次下载 · ${Number(model.likes || 0).toLocaleString()} 个赞${model.pipelineTag ? ` · ${model.pipelineTag}` : ""}`;
      details.append(repo, meta);
      const button = document.createElement("button"); button.className = "button secondary"; button.textContent = "查看 GGUF 文件";
      button.addEventListener("click", () => loadHfFiles(model.repoId));
      card.append(details, button); box.append(card);
    });
  }
  async function loadHfFiles(repoId) {
    const box = $("#hf-files"); box.hidden = false; box.replaceChildren();
    const heading = document.createElement("h3"); heading.textContent = `${repoId} · GGUF 文件`;
    const loading = document.createElement("p"); loading.className = "muted"; loading.textContent = "正在读取文件列表…"; box.append(heading, loading);
    try {
      const files = await invoke("huggingface_files", { repoId });
      box.replaceChildren(heading);
      if (!files.length) { const empty = document.createElement("p"); empty.textContent = "此仓库没有可识别的 GGUF 文件。"; box.append(empty); return; }
      const select = document.createElement("select");
      select.setAttribute("aria-label", "选择 GGUF 模型文件");
      files.forEach((file) => select.add(new Option(`${file.name} · ${file.sizeBytes ? fmtBytes(file.sizeBytes) : "大小未知"}`, file.name)));
      const actions = document.createElement("div"); actions.className = "hf-file-actions";
      const download = document.createElement("button"); download.className = "button primary"; download.textContent = "下载到本机";
      const progress = document.createElement("div"); progress.className = "download-progress"; progress.hidden = true;
      download.addEventListener("click", async () => {
        download.disabled = true; progress.hidden = false; progress.textContent = "开始下载…";
        try {
          await invoke("download_huggingface_model", { repoId, fileName: select.value });
          await loadModels(); progress.textContent = "下载完成并保存到本机。"; showToast("模型下载完成");
        } catch (error) { progress.textContent = `下载失败：${error}`; }
        finally { download.disabled = false; }
      });
      actions.append(download); box.append(select, actions, progress);
    } catch (error) { loading.textContent = `读取文件失败：${error}`; }
  }
  async function searchHf(event) {
    event.preventDefault();
    const button = $("#model-search-form button[type=submit]");
    button.disabled = true; button.textContent = "搜索中…";
    $("#hf-files").hidden = true;
    $("#hf-results").innerHTML = `<div class="empty-state">正在查询 Hugging Face…</div>`;
    try { renderHfModels(await invoke("search_huggingface", { query: $("#model-search-query").value })); }
    catch (error) { $("#hf-results").innerHTML = `<div class="error-box">搜索失败：${escapeText(error)}</div>`; }
    finally { button.disabled = false; button.textContent = "搜索"; }
  }

  async function refreshRuntime() {
    try {
      state.runtime = await invoke("runtime_status");
      const host = state.runtime.bindHost || "127.0.0.1";
      const label = state.runtime.running ? `模型运行中 · ${state.runtime.modelName || "本地模型"}` : "模型未启动";
      $("#runtime-chip").textContent = label;
      $("#runtime-chip").classList.toggle("running", state.runtime.running);
      $("#run-status").textContent = state.runtime.running ? `运行中：${state.runtime.modelName} · ${host}:${state.runtime.port}` : "模型未启动";
      if (state.runtime.lastError) {
        $("#run-error").textContent = state.runtime.lastError;
        $("#run-error").hidden = false;
      } else if (!state.runtime.running) $("#run-error").hidden = true;
      renderRunCapabilityStatus();
      $("#stop-model").disabled = !state.runtime.running;
      $("#chat-model-state").textContent = state.runtime.running
        ? `运行中 · ${state.runtime.modelName || "本地模型"}`
        : (state.runtime.lastError ? "服务异常 · 未启动" : "未启动");
      $("#chat-local-port").innerHTML = `<i></i>${host === "127.0.0.1" ? "本机运行 · 不联网" : "本机服务 · 局域网开放"}`;
      $("#chat-local-port").dataset.lan = String(host !== "127.0.0.1");
      $("#chat-local-port").title = `本地地址 ${host}:${state.runtime.port}；${host === "127.0.0.1" ? "服务仅对本机开放" : "运行页已允许私有网络设备访问"}`;
      const chatModelSelect = $("#chat-model-select");
      if (state.runtime.running && [...chatModelSelect.options].some((option) => option.value === state.runtime.modelName)) {
        chatModelSelect.value = state.runtime.modelName;
      }
      chatModelSelect.disabled = !state.models.length || state.busyChat;
      $("#chat-input").disabled = !state.runtime.running || state.busyChat;
      updateChatSendState();
      $("#chat-hint").textContent = state.runtime.running
        ? "发送到本机 llama-server；回复为非流式"
        : (state.models.length ? "选择模型将启动本机推理服务" : "先导入一个 GGUF 模型");
      $("#monitor-api").textContent = `${host}:${state.runtime.port}`;
    } catch { /* Not available while the frontend is opened outside the app. */ }
  }

  async function startModel() {
    const modelName = $("#run-model-select").value;
    if (!modelName) { showToast("请先选择一个 GGUF 模型", "error"); return; }
    const button = $("#start-model"); button.disabled = true; button.textContent = "启动中…";
    $("#run-error").hidden = true;
    $("#run-status").textContent = "正在启动本机 llama-server；加载时间取决于模型大小…";
    try { state.runtime = await invoke("start_local_model", { modelName, options: effectiveRunOptions() }); showToast("本地模型已就绪"); }
    catch (error) { $("#run-error").textContent = String(error); $("#run-error").hidden = false; }
    finally { button.textContent = "启动模型"; await refreshRuntime(); }
  }
  async function stopModel() {
    $("#stop-model").disabled = true;
    try { state.runtime = await invoke("stop_local_model"); showToast("本地模型已停止"); }
    catch (error) { $("#run-error").textContent = String(error); $("#run-error").hidden = false; }
    finally { await refreshRuntime(); }
  }

  async function selectChatModel() {
    const select = $("#chat-model-select");
    const modelName = select.value;
    if (!modelName) return;
    if (state.runtime.running && state.runtime.modelName === modelName) return;
    if (state.runtime.running && !window.confirm(`切换到“${modelName}”会停止当前模型并加载新模型，可能需要等待并占用较多内存。继续吗？`)) {
      select.value = state.runtime.modelName || "";
      return;
    }
    select.disabled = true;
    try {
      if (state.runtime.running) await invoke("stop_local_model");
      state.pendingRunModel = modelName;
      $("#run-model-select").value = modelName;
      state.runtime = await invoke("start_local_model", { modelName, options: effectiveRunOptions() });
      showToast(`本地模型已启动：${modelName}`);
    } catch (error) {
      showToast(`无法启动所选模型：${error}`, "error");
    } finally {
      await refreshRuntime();
      select.disabled = !state.models.length || state.busyChat;
    }
  }

  function renderChat() {
    const session = ensureSession();
    const list = $("#chat-sessions");
    list.replaceChildren();
    const filter = $("#chat-filter").value.trim().toLowerCase();
    const mode = $("#chat-filter-mode").value;
    const weekAgo = Date.now() - 7 * 24 * 60 * 60 * 1000;
    const visibleSessions = state.sessions
      .filter((item) => mode !== "recent" || Number(item.updatedAt || 0) >= weekAgo)
      .filter((item) => mode !== "pinned" || item.pinned)
      .filter((item) => {
        if (!filter) return true;
        return String(item.title || "新对话").toLowerCase().includes(filter)
          || (item.messages || []).some((message) => `${message.content || ""} ${(message.attachments || []).map((attachment) => `${attachment.name || ""} ${attachment.text || ""}`).join(" ")}`.toLowerCase().includes(filter));
      })
      .sort((left, right) => Number(Boolean(right.pinned)) - Number(Boolean(left.pinned)) || Number(right.updatedAt || 0) - Number(left.updatedAt || 0));
    visibleSessions.forEach((item) => {
      const row = document.createElement("div");
      row.className = `session-item-row${item.id === state.currentSessionId ? " active" : ""}`;
      const open = document.createElement("button");
      open.type = "button"; open.className = "session-item-main";
      const title = document.createElement("strong"); title.textContent = item.title || "新对话";
      const date = document.createElement("small");
      date.textContent = Number.isFinite(Number(item.updatedAt)) ? new Date(Number(item.updatedAt)).toLocaleDateString() : "";
      open.append(title, date); open.title = title.textContent;
      open.addEventListener("click", () => {
        state.currentSessionId = item.id;
        try { localStorage.setItem(CURRENT_CHAT_KEY, item.id); } catch { /* Native history remains primary. */ }
        renderChat();
      });
      const pin = document.createElement("button");
      pin.type = "button"; pin.className = "session-action"; pin.textContent = item.pinned ? "★" : "☆";
      pin.title = item.pinned ? "取消置顶" : "置顶对话"; pin.setAttribute("aria-pressed", String(Boolean(item.pinned)));
      pin.addEventListener("click", () => { item.pinned = !item.pinned; saveSessions(); renderChat(); });
      const rename = document.createElement("button");
      rename.type = "button"; rename.className = "session-action"; rename.textContent = "···"; rename.title = "重命名对话";
      rename.addEventListener("click", () => {
        const updated = window.prompt("对话名称", item.title || "新对话");
        if (updated === null || !updated.trim()) return;
        item.title = updated.trim().slice(0, 200); item.updatedAt = Date.now(); saveSessions(); renderChat();
      });
      const remove = document.createElement("button");
      remove.type = "button"; remove.className = "session-action delete-session"; remove.textContent = "×"; remove.title = "删除对话";
      remove.addEventListener("click", () => {
        if (!window.confirm(`删除对话“${item.title || "新对话"}”？此操作会移除此设备保存的对话历史。`)) return;
        state.sessions = state.sessions.filter((entry) => entry.id !== item.id);
        if (state.currentSessionId === item.id) state.currentSessionId = state.sessions[0]?.id || null;
        saveSessions(); renderChat();
      });
      row.append(open, pin, rename, remove); list.append(row);
    });
    if (!visibleSessions.length) {
      const empty = document.createElement("p"); empty.className = "chat-history-empty";
      empty.textContent = filter || mode !== "all" ? "没有符合条件的对话" : "还没有历史对话"; list.append(empty);
    }

    const pane = $("#chat-messages"); pane.replaceChildren();
    if (!session.messages.length) {
      const welcome = document.createElement("div"); welcome.className = "welcome";
      welcome.innerHTML = `<div class="welcome-mark">◇</div><h2>有什么可以帮你？</h2><p>模型完全在你的电脑上运行；聊天内容不会上传。选择一个已安装的本地模型后即可开始。</p><a class="button secondary" href="#run">前往运行管理</a>`;
      pane.append(welcome);
    } else {
      session.messages.forEach((message, index) => {
        const row = document.createElement("article");
        row.className = `message ${message.role === "user" ? "user" : "assistant"}${message.error ? " message-error" : ""}`;
        const label = document.createElement("small"); label.textContent = message.error ? "请求失败 · 本地服务" : (message.role === "user" ? "你" : "本地模型");
        const content = document.createElement("div"); content.className = "message-content"; content.textContent = message.content || "";
        row.append(label, content);
        if (message.attachments?.length) {
          const attachmentList = document.createElement("div"); attachmentList.className = "message-attachment-list";
          message.attachments.forEach((attachment) => {
            const chip = document.createElement("span"); chip.textContent = `附件 · ${attachment.name}`; chip.title = "附件文本仅保存在本机对话中"; attachmentList.append(chip);
          });
          row.append(attachmentList);
        }
        const actions = document.createElement("div"); actions.className = "message-actions";
        const addAction = (text, titleText, handler, danger = false) => {
          const button = document.createElement("button"); button.type = "button"; button.textContent = text; button.title = titleText;
          if (danger) button.classList.add("message-delete-action");
          button.addEventListener("click", handler); actions.append(button);
        };
        if (message.error) addAction("重试", "重新发送本地推理请求", () => retryChatMessage(session, index));
        else if (message.role === "user") {
          addAction("编辑", "编辑这条用户消息并重新生成后续回答", () => editChatMessage(session, index));
          addAction("复制", "复制消息文本", () => copyChatText(message.content));
        } else {
          addAction("复制", "复制消息文本", () => copyChatText(message.content));
          addAction("重新生成", "重新生成这条回答", () => regenerateChatMessage(session, index));
          addAction("调整回答", "根据新指示继续调整回答", () => adjustChatAnswer(session, index));
        }
        addAction("删除", "从本机历史中删除这条消息", () => deleteChatMessage(session, index), true);
        row.append(actions); pane.append(row);
      });
      if (state.busyChat && state.busySessionId === session.id) {
        const loading = document.createElement("div"); loading.className = "typing-indicator";
        loading.textContent = "本地模型正在生成回复（非流式）…"; pane.append(loading);
      }
    }
    pane.scrollTop = pane.scrollHeight;
    renderDraftAttachments();
  }

  function messageToApi(message) {
    let content = String(message.content || "");
    for (const attachment of message.attachments || []) content += `\n\n[本地附件：${attachment.name}]\n${attachment.text}`;
    if (new TextEncoder().encode(content).length > 32000) throw new Error("单条消息及附件文本不能超过 32 KB；请移除或缩短附件。");
    return { role: message.role, content };
  }

  function apiHistory(session) {
    let messages = session.messages.filter((message) => !message.error && ["user", "assistant"].includes(message.role));
    if (!state.chatPreferences.includeHistory) messages = messages.slice(-1);
    return messages.slice(-60).map(messageToApi);
  }

  async function requestChatReply(session) {
    if (!state.runtime.running) { showToast("请先选择并启动一个本地模型", "error"); return; }
    if (state.busyChat) return;
    state.busyChat = true; state.busySessionId = session.id;
    renderChat(); await refreshRuntime();
    try {
      const messages = apiHistory(session);
      const result = await invoke("chat_complete", { messages, temperature: state.chatPreferences.temperature });
      session.messages.push({ role: "assistant", content: String(result), createdAt: Date.now() });
      session.updatedAt = Date.now(); saveSessions();
    } catch (error) {
      session.messages.push({ role: "assistant", content: String(error), error: true, createdAt: Date.now() });
      session.updatedAt = Date.now(); saveSessions();
    } finally {
      state.busyChat = false; state.busySessionId = null; renderChat(); await refreshRuntime();
    }
  }

  async function sendChat(event) {
    event.preventDefault();
    const text = $("#chat-input").value.trim();
    if ((!text && !state.draftAttachments.length) || !state.runtime.running || state.busyChat) return;
    const session = ensureSession();
    const message = {
      role: "user",
      content: text || "请阅读附件内容并帮我分析。",
      attachments: state.draftAttachments.map((attachment) => ({ name: attachment.name, text: attachment.text })),
      createdAt: Date.now(),
    };
    try { messageToApi(message); } catch (error) { showToast(String(error), "error"); return; }
    session.messages.push(message);
    if (session.messages.filter((item) => item.role === "user").length === 1) session.title = (text || message.attachments[0]?.name || "新对话").slice(0, 42);
    session.updatedAt = Date.now(); $("#chat-input").value = ""; state.draftAttachments = [];
    saveSessions(); renderChat(); updateChatSendState();
    await requestChatReply(session);
  }

  async function copyChatText(text) {
    try {
      if (navigator.clipboard?.writeText) await navigator.clipboard.writeText(String(text || ""));
      else {
        const field = document.createElement("textarea"); field.value = String(text || ""); field.style.position = "fixed"; field.style.opacity = "0"; document.body.append(field); field.select();
        const copied = document.execCommand("copy"); field.remove(); if (!copied) throw new Error("系统剪贴板不可用");
      }
      showToast("消息已复制");
    } catch (error) { showToast(`复制失败：${error}`, "error"); }
  }

  async function retryChatMessage(session, index) {
    if (state.busyChat) return;
    if (!state.runtime.running) { showToast("请先启动本地模型，再重试此请求", "error"); return; }
    session.messages.splice(index, 1); session.updatedAt = Date.now(); saveSessions();
    await requestChatReply(session);
  }

  async function regenerateChatMessage(session, index) {
    if (state.busyChat) return;
    if (!state.runtime.running) { showToast("请先启动本地模型，再重新生成回答", "error"); return; }
    if (index < session.messages.length - 1 && !window.confirm("重新生成这条回答会移除它之后的消息。继续吗？")) return;
    session.messages.splice(index); session.updatedAt = Date.now(); saveSessions();
    await requestChatReply(session);
  }

  async function editChatMessage(session, index) {
    if (state.busyChat) return;
    if (!state.runtime.running) { showToast("请先启动本地模型，再编辑并重答", "error"); return; }
    const updated = window.prompt("编辑你的消息", session.messages[index].content || "");
    if (updated === null || !updated.trim()) return;
    if (index < session.messages.length - 1 && !window.confirm("编辑后会移除这条消息之后的回答和消息，并重新请求模型。继续吗？")) return;
    session.messages[index].content = updated.trim(); session.messages.splice(index + 1);
    session.updatedAt = Date.now(); saveSessions(); await requestChatReply(session);
  }

  async function adjustChatAnswer(session, index) {
    if (state.busyChat) return;
    if (!state.runtime.running) { showToast("请先启动本地模型，再调整回答", "error"); return; }
    const instruction = window.prompt("你希望模型怎样调整这条回答？", "请更简洁地回答");
    if (!instruction?.trim()) return;
    const original = session.messages[index]?.content || "";
    const request = `请调整下面这条回答。\n\n原回答：\n${original}\n\n调整要求：\n${instruction.trim()}`;
    if (new TextEncoder().encode(request).length > 32000) { showToast("所选回答过长，无法在本地 API 的单条消息限制内再次调整", "error"); return; }
    session.messages.push({ role: "user", content: request, createdAt: Date.now() });
    session.updatedAt = Date.now(); saveSessions(); await requestChatReply(session);
  }

  function deleteChatMessage(session, index) {
    if (state.busyChat) return;
    if (!window.confirm("从本机对话历史中删除这条消息？")) return;
    session.messages.splice(index, 1); session.updatedAt = Date.now(); saveSessions(); renderChat();
  }

  function renderDraftAttachments() {
    const box = $("#chat-attachments"); box.replaceChildren();
    state.draftAttachments.forEach((attachment, index) => {
      const chip = document.createElement("span"); chip.className = "draft-attachment";
      const name = document.createElement("span"); name.textContent = `${attachment.name} · 文本`;
      const remove = document.createElement("button"); remove.type = "button"; remove.textContent = "×"; remove.title = `移除 ${attachment.name}`;
      remove.addEventListener("click", () => { state.draftAttachments.splice(index, 1); renderDraftAttachments(); updateChatSendState(); });
      chip.append(name, remove); box.append(chip);
    });
  }

  async function addChatAttachments(files) {
    const allowed = /\.(txt|md|markdown|csv|tsv|json|jsonl|log|yaml|yml|toml|xml|html|css|js|ts|tsx|jsx|rs|py|go|java|c|cpp|h|sql)$/i;
    if (state.draftAttachments.length + files.length > 5) { showToast("最多同时附加 5 个文本文件", "error"); return; }
    let totalBytes = state.draftAttachments.reduce((sum, item) => sum + new TextEncoder().encode(item.text).length, 0);
    for (const file of files) {
      if (!allowed.test(file.name)) { showToast(`不支持“${file.name}”：仅接受常见纯文本、文档和代码文件；图片/PDF 暂不支持`, "error"); continue; }
      if (file.size > 2 * 1024 * 1024) { showToast(`“${file.name}”超过 2 MiB 附件上限`, "error"); continue; }
      try {
        const buffer = await file.arrayBuffer();
        const text = new TextDecoder("utf-8", { fatal: true }).decode(buffer);
        const size = new TextEncoder().encode(text).length;
        if (totalBytes + size > 24000) { showToast("附件文本总量最多 24 KB，以符合本地服务单条消息限制", "error"); continue; }
        totalBytes += size; state.draftAttachments.push({ name: file.name, text });
      } catch { showToast(`无法按 UTF-8 文本读取“${file.name}”`, "error"); }
    }
    renderDraftAttachments(); updateChatSendState();
  }
  async function loadSettings() {
    try {
      state.settings = await invoke("app_settings");
      $("#settings-model-dir").value = state.settings.modelDirectory;
      $("#settings-appearance").value = state.settings.appearance || "system";
      $("#settings-port").value = state.settings.apiPort || 8686;
      applyAppearance(state.settings.appearance);
    } catch (error) { $("#settings-status").textContent = `无法读取设置：${error}`; }
  }
  function applyAppearance(value) {
    document.documentElement.dataset.theme = value === "dark" ? "dark" : (value === "light" ? "light" : "system");
  }
  async function saveSettings(event) {
    event.preventDefault();
    const settings = {
      modelDirectory: $("#settings-model-dir").value.trim(),
      appearance: $("#settings-appearance").value,
      apiPort: Number($("#settings-port").value),
    };
    const status = $("#settings-status"); status.textContent = "正在保存…";
    try {
      state.settings = await invoke("save_app_settings", { settings });
      status.textContent = "已保存"; applyAppearance(settings.appearance); await refreshRuntime(); await loadModels();
      showToast("设置已保存");
    } catch (error) { status.textContent = `保存失败：${error}`; }
  }
  async function chooseModelDirectory() {
    try {
      const path = await invoke("choose_directory");
      if (path) $("#settings-model-dir").value = path;
    } catch (error) { showToast(`选择目录失败：${error}`, "error"); }
  }

  async function refreshSystem() {
    try {
      const sample = await invoke("system_snapshot");
      const cpu = Math.max(0, Math.min(100, sample.cpuUsagePercent || 0));
      const ram = sample.totalMemoryBytes ? sample.usedMemoryBytes / sample.totalMemoryBytes * 100 : 0;
      const item = { at: Date.now(), cpu, ram: Math.max(0, Math.min(100, ram)) };
      state.samples.push(item);
      state.samples = state.samples.filter((entry) => entry.at > Date.now() - 120_000).slice(-80);
      $("#monitor-cpu-value").textContent = `${cpu.toFixed(0)}%`;
      $("#monitor-cpu-bar").style.width = `${cpu}%`;
      $("#monitor-ram-value").textContent = `${ram.toFixed(0)}%`;
      $("#monitor-ram-bar").style.width = `${ram}%`;
      $("#monitor-ram-text").textContent = `${fmtBytes(sample.usedMemoryBytes)} / ${fmtBytes(sample.totalMemoryBytes)} · 可用 ${fmtBytes(sample.availableMemoryBytes)}`;
      $("#overview-cpu").textContent = `${cpu.toFixed(0)}%`;
      $("#overview-cpu-bar").style.width = `${cpu}%`;
      $("#overview-ram").textContent = `${fmtBytes(sample.usedMemoryBytes)} / ${fmtBytes(sample.totalMemoryBytes)}`;
      $("#overview-ram-bar").style.width = `${ram}%`;
      if ($("#page-monitor").classList.contains("active")) drawChart();
    } catch { /* The app can still render when Tauri APIs are unavailable. */ }
  }
  function drawChart() {
    const canvas = $("#system-chart");
    if (!canvas) return;
    const rect = canvas.getBoundingClientRect();
    if (!rect.width) return;
    const scale = window.devicePixelRatio || 1;
    canvas.width = Math.max(1, Math.floor(rect.width * scale));
    canvas.height = Math.floor(300 * scale);
    const ctx = canvas.getContext("2d");
    const width = canvas.width; const height = canvas.height;
    ctx.clearRect(0, 0, width, height);
    ctx.strokeStyle = getComputedStyle(document.documentElement).getPropertyValue("--line").trim() || "#e5ebe8";
    ctx.lineWidth = scale;
    ctx.fillStyle = getComputedStyle(document.documentElement).getPropertyValue("--muted").trim() || "#82908a";
    ctx.font = `${11 * scale}px system-ui`;
    for (let i = 0; i <= 4; i++) {
      const y = height - (i / 4) * height * 0.85 - height * 0.06;
      ctx.beginPath(); ctx.moveTo(38 * scale, y); ctx.lineTo(width - 12 * scale, y); ctx.stroke();
      ctx.fillText(`${i * 25}%`, 2 * scale, y + 4 * scale);
    }
    const values = state.samples;
    const plotLeft = 42 * scale; const plotRight = width - 12 * scale; const plotTop = height * 0.06; const plotBottom = height * 0.91;
    const drawLine = (key, color) => {
      if (values.length < 2) return;
      ctx.strokeStyle = color; ctx.lineWidth = 2.5 * scale; ctx.beginPath();
      values.forEach((sample, index) => {
        const x = plotLeft + (index / Math.max(1, values.length - 1)) * (plotRight - plotLeft);
        const y = plotBottom - sample[key] / 100 * (plotBottom - plotTop);
        if (!index) ctx.moveTo(x, y); else ctx.lineTo(x, y);
      }); ctx.stroke();
    };
    drawLine("cpu", "#d87556"); drawLine("ram", "#4c82bb");
  }

  function switchModelTab(tab) {
    $$("[data-model-tab]").forEach((button) => button.classList.toggle("active", button.dataset.modelTab === tab));
    $("#models-recommended").hidden = tab !== "recommended";
    $("#models-installed").hidden = tab !== "installed";
    $("#models-media").hidden = tab !== "media";
    $("#models-download").hidden = tab !== "download";
  }

  function switchImageView(view) {
    const canvas = view === "canvas";
    $$("[data-image-view]").forEach((button) => {
      const active = button.dataset.imageView === (canvas ? "canvas" : "form");
      button.classList.toggle("active", active);
      button.setAttribute("aria-selected", String(active));
    });
    $("#image-form-view").hidden = canvas;
    $("#image-canvas-view").hidden = !canvas;
  }

  async function openCreationsDirectory(kind, button) {
    button.disabled = true;
    try {
      await invoke("open_creations_directory", { kind });
      showToast(`${kind === "image" ? "画图" : "视频"}作品文件夹已打开`);
    } catch (error) { showToast(`打开作品文件夹失败：${error}`, "error"); }
    finally { button.disabled = false; }
  }

  async function start() {
    if (!tauri?.core?.invoke || !listen) {
      $("#overview-headline").textContent = "请在桌面应用中打开";
      $("#overview-copy").textContent = "本页面依赖本机 Tauri 服务，不能在普通浏览器中单独使用。";
      return;
    }
    try {
      renderRecommendedModels();
      await listen("engine-progress", (event) => {
        const progress = event.payload;
        $$(".engine-card").filter((card) => (card.dataset.engine === "llama.cpp" ? "llama.cpp" : "stable-diffusion.cpp") === progress.engine)
          .forEach((card) => { const box = $("[data-progress]", card); box.hidden = false; box.textContent = progress.phase === "download" ? `下载中 ${progress.total ? Math.round(progress.completed / progress.total * 100) : 0}% · ${fmtBytes(progress.completed)} / ${fmtBytes(progress.total)}` : progress.phase === "verify" ? "正在校验 SHA-256…" : progress.phase === "extract" ? `安全解压 ${progress.completed} / ${progress.total}` : "安装完成"; });
        if (progress.engine === "stable-diffusion.cpp") {
          const message = progress.phase === "download" ? `下载中 ${progress.total ? Math.round(progress.completed / progress.total * 100) : 0}% · ${fmtBytes(progress.completed)} / ${fmtBytes(progress.total)}` : progress.phase === "verify" ? "正在校验 SHA-256…" : progress.phase === "extract" ? `安全解压 ${progress.completed} / ${progress.total}` : "安装完成";
          ["image", "video"].forEach((page) => { const box = $(`#${page}-engine-progress`); if (box) { box.hidden = false; box.textContent = message; } });
        }
      });
      await listen("model-progress", (event) => {
        const payload = event.payload;
        const progress = $(".download-progress:not([hidden])");
        if (progress) progress.textContent = payload.total ? `下载中 ${Math.floor(payload.completed / payload.total * 100)}% · ${fmtBytes(payload.completed)} / ${fmtBytes(payload.total)}` : `已下载 ${fmtBytes(payload.completed)}`;
      });
      renderRunPreferences();
      $("#chat-temperature").value = String(state.chatPreferences.temperature);
      $("#chat-temperature-value").textContent = state.chatPreferences.temperature.toFixed(1);
      $("#chat-include-history").checked = state.chatPreferences.includeHistory;
      await Promise.all([loadChatHistory(), loadHardware(), loadEngines(), loadModels(), loadSettings(), refreshRuntime(), refreshSystem()]);
      setInterval(refreshSystem, 2000);
      setInterval(refreshRuntime, 3000);
      setPage(location.hash.slice(1) || "overview");
    } catch (error) { showToast(`桌面服务初始化失败：${error}`, "error"); }
  }

  window.addEventListener("hashchange", () => setPage(location.hash.slice(1)));
  window.addEventListener("resize", () => { if ($("#page-monitor").classList.contains("active")) drawChart(); });
  $("#refresh-hardware").addEventListener("click", loadHardware);
  $("#refresh-engines").addEventListener("click", loadEngines);
  $("#pick-model").addEventListener("click", importLocalModel);
  $("#model-search-form").addEventListener("submit", searchHf);
  $$("[data-model-tab]").forEach((button) => button.addEventListener("click", () => switchModelTab(button.dataset.modelTab)));
  $$("[data-image-view]").forEach((button) => button.addEventListener("click", () => switchImageView(button.dataset.imageView)));
  $("#open-creations-directory").addEventListener("click", (event) => openCreationsDirectory("image", event.currentTarget));
  $("#open-video-directory").addEventListener("click", (event) => openCreationsDirectory("video", event.currentTarget));
  $$("[data-media-install]").forEach((button) => button.addEventListener("click", installMediaEngine));
  $("#start-model").addEventListener("click", startModel);
  $("#stop-model").addEventListener("click", stopModel);
  $("#run-model-select").addEventListener("change", () => { updateRunEstimates(); refreshRuntime(); });
  $$('input[name="run-context"]').forEach((input) => input.addEventListener("change", (event) => {
    if (event.target.checked) updateRunPreference("context", event.target.value);
  }));
  $("#flash-attention").addEventListener("change", (event) => updateRunPreference("flashAttention", event.target.value));
  $("#kv-cache-q8").addEventListener("change", (event) => updateRunPreference("kvCacheQ8", event.target.checked));
  $("#auto-restart").addEventListener("change", (event) => updateRunPreference("autoRestart", event.target.checked));
  $("#lan-access").addEventListener("change", (event) => {
    updateRunPreference("lanAccess", event.target.checked);
    renderLanAccessNote();
  });
  $("#new-chat").addEventListener("click", createSession);
  $("#clear-chat").addEventListener("click", () => {
    const session = currentSession();
    if (!session || !session.messages.length) return;
    if (!window.confirm("清空当前对话中的所有消息？此操作不可撤销。")) return;
    session.messages = []; session.title = "新对话"; session.updatedAt = Date.now(); saveSessions(); renderChat();
  });
  $("#chat-form").addEventListener("submit", sendChat);
  $("#chat-input").addEventListener("keydown", (event) => { if (event.key === "Enter" && !event.shiftKey) { event.preventDefault(); $("#chat-form").requestSubmit(); } });
  $("#chat-input").addEventListener("input", updateChatSendState);
  $("#chat-filter").addEventListener("input", renderChat);
  $("#chat-filter-mode").addEventListener("change", renderChat);
  $("#chat-model-select").addEventListener("change", selectChatModel);
  $("#attach-files").addEventListener("click", () => $("#chat-file-input").click());
  $("#chat-file-input").addEventListener("change", async (event) => {
    await addChatAttachments([...event.target.files]); event.target.value = "";
  });
  $("#toggle-chat-sidebar").addEventListener("click", () => {
    const layout = $(".chat-layout");
    const collapsed = layout.classList.toggle("sidebar-collapsed");
    $("#toggle-chat-sidebar").title = collapsed ? "展开对话列表" : "收起对话列表";
    $("#toggle-chat-sidebar").setAttribute("aria-label", $("#toggle-chat-sidebar").title);
  });
  $("#chat-temperature").addEventListener("input", (event) => {
    state.chatPreferences.temperature = Number(event.target.value);
    $("#chat-temperature-value").textContent = state.chatPreferences.temperature.toFixed(1);
    saveChatPreferences();
  });
  $("#chat-include-history").addEventListener("change", (event) => {
    state.chatPreferences.includeHistory = event.target.checked; saveChatPreferences();
  });
  $("#settings-form").addEventListener("submit", saveSettings);
  $("#choose-model-dir").addEventListener("click", chooseModelDirectory);
  $("#settings-appearance").addEventListener("change", (event) => applyAppearance(event.target.value));
  $("#monitor-refresh").addEventListener("click", refreshSystem);
  $$("[data-jump]").forEach((button) => button.addEventListener("click", () => { location.hash = `#${button.dataset.jump}`; }));
  start();
})();
