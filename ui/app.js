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
    storage: null,
    engines: new Map(),
    enginesLoaded: false,
    mediaInstallSupported: false,
    mediaInstallBusy: false,
    models: [],
    recommendationCategory: "fit",
    recommendationSeq: 0,
    recommendationsLoaded: false,
    hfCategory: "chat",
    hfSearchSeq: 0,
    currentDownloadId: null,
    mediaModels: [],
    mediaGenerationJobId: null,
    mediaGenerationKind: null,
    comfyuiScan: null,
    comfyuiScanPath: null,
    comfyuiScanError: "",
    pendingRunModel: null,
    runtime: { running: false, port: 8686, modelName: null },
    runCapabilities: null,
    runCapabilitiesLoading: null,
    runPreferences: loadRunPreferences(),
    samples: [],
    modelProcesses: [],
    scheduledTasks: [],
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
    if (safePage === "models") {
      loadModels();
      renderMediaLibraryEngine();
      refreshMediaModels();
      if (!$("#models-recommended").hidden && !state.recommendationsLoaded) searchRecommendationModels(state.recommendationCategory);
      if (state.settings?.comfyuiDirectory && state.comfyuiScanPath !== state.settings.comfyuiDirectory) scanComfyUiDirectory(state.settings.comfyuiDirectory);
    }
    if (safePage === "run") { loadEngines(); refreshRuntime(); loadModels(); loadRunCapabilities(true); }
    if (safePage === "image" || safePage === "video") {
      renderMediaEngineBanners();
      refreshMediaModels();
    }
    if (safePage === "chat") { renderChat(); refreshRuntime(); }
    if (safePage === "monitor") { drawChart(); refreshSystem(); refreshRuntime(); }
    if (safePage === "schedule") loadScheduledTasks();
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
      $("#estimated-speed").textContent = "未实测";
      $("#estimated-load").textContent = "未实测";
      $("#run-gpu-bar").style.width = "0%";
      $("#run-ram-bar").style.width = "0%";
      $("#run-disk-bar").style.width = "0%";
      return;
    }
    const sizeGiB = Math.max(model.sizeBytes / (1024 ** 3), 0.05);
    const hardware = state.hardware || {};
    const totalBytes = Number(hardware.totalMemoryBytes) || 0;
    const availableBytes = Number(hardware.availableMemoryBytes) || 0;
    const availableGiB = availableBytes / (1024 ** 3);
    const totalGiB = totalBytes / (1024 ** 3);
    const memoryDetected = totalBytes > 0;
    const ramPercent = memoryDetected ? Math.min(100, model.sizeBytes / totalBytes * 100) : 0;
    const diskTotalBytes = Number(state.storage?.totalBytes) || 0;
    const diskAvailableBytes = Number(state.storage?.availableBytes);
    const diskDetected = diskTotalBytes > 0 && Number.isFinite(diskAvailableBytes);
    const diskPercent = diskDetected && diskAvailableBytes > 0
      ? Math.min(100, model.sizeBytes / diskAvailableBytes * 100)
      : 0;
    $("#run-plan-summary").textContent = !memoryDetected
      ? `${model.name} · 模型文件约 ${sizeGiB.toFixed(1)} GB；未检测到可用内存数据，无法估算内存压力。`
      : availableBytes >= model.sizeBytes
        ? `${model.name} · 文件约 ${sizeGiB.toFixed(1)} GB，小于当前可用内存；运行仍需要额外内存，不能据此保证加载成功。`
        : `${model.name} · 文件约 ${sizeGiB.toFixed(1)} GB，大于当前可用内存；可能依赖磁盘映射，速度与稳定性无法保证。`;
    $("#run-gpu-value").textContent = hardware.gpuName ? "已识别图形设备" : "图形设备信息未知";
    $("#run-gpu-note").textContent = hardware.gpuName
      ? `${hardware.gpuName}；当前版本尚未接入 GPU 加速选择。`
      : "未检测到可用图形设备信息；不能据此确认设备中没有 GPU。";
    $("#run-ram-value").textContent = memoryDetected
      ? `${fmtBytes(availableBytes)} 可用 / ${fmtBytes(totalBytes)} 总计 · 模型约 ${sizeGiB.toFixed(1)} GB`
      : `内存数据未知 · 模型约 ${sizeGiB.toFixed(1)} GB`;
    $("#run-ram-note").textContent = memoryDetected
      ? `条形参考为模型文件大小 / 系统内存总量；实际运行内存还受上下文和缓存影响。`
      : "未检测到系统内存数据，无法评估模型运行内存。";
    $("#run-disk-value").textContent = diskDetected
      ? `${sizeGiB.toFixed(1)} GB 文件 · 磁盘剩余 ${fmtBytes(diskAvailableBytes)}`
      : `${sizeGiB.toFixed(1)} GB 文件 · 磁盘余量未知`;
    $("#run-disk-note").textContent = diskDetected
      ? `条形参考为模型文件大小 / 模型目录所在磁盘当前可用空间；模型仍保留在本机。`
      : "未检测到模型目录所在磁盘的空间信息，无法判断剩余容量。";
    $("#run-gpu-bar").style.width = "0%";
    $("#run-ram-bar").style.width = `${ramPercent}%`;
    $("#run-disk-bar").style.width = `${diskPercent}%`;
    $("#estimated-speed").textContent = "未实测";
    $("#estimated-load").textContent = "未实测";
  }

  function renderOverviewHardware() {
    const hw = state.hardware;
    const totalBytes = Number(hw?.totalMemoryBytes) || 0;
    const availableBytes = Number(hw?.availableMemoryBytes) || 0;
    const logicalCores = Number(hw?.logicalCores) || 0;
    $("#hardware-cpu").textContent = hw?.cpuName || "未检测到处理器信息";
    $("#hardware-cores").textContent = logicalCores
      ? `${logicalCores} 个逻辑处理器 · ${hw.os || "系统未知"} · ${hw.architecture || "架构未知"}`
      : `处理器核心数未知 · ${hw?.os || "系统未知"} · ${hw?.architecture || "架构未知"}`;
    $("#hardware-memory").textContent = totalBytes > 0 ? fmtBytes(totalBytes) : "内存容量未知";
    $("#memory-free").textContent = totalBytes > 0 ? `可用 ${fmtBytes(availableBytes)}` : "可用内存未知";
    $("#hardware-gpu").textContent = hw?.gpuName || "未检测到可用图形设备信息";
    $("#gpu-memory").textContent = Number(hw?.gpuMemoryBytes) > 0
      ? `显存 ${fmtBytes(Number(hw.gpuMemoryBytes))}`
      : "显存容量未知；当前版本未提供 GPU 加速选择";
    $("#overview-headline").textContent = hw?.gpuName
      ? `识别到图形设备：${hw.gpuName}`
      : "图形设备信息未检测到";
    $("#overview-copy").textContent = hw
      ? `处理器：${hw.cpuName || "未知"}；${totalBytes > 0 ? `系统内存 ${fmtBytes(totalBytes)}，当前可用 ${fmtBytes(availableBytes)}` : "系统内存数据未知"}。本版本不预设生成速度，也未提供 GPU 加速选择。`
      : "硬件检测失败；未取得的配置不会用示例数值代替。";
    $("#overview-memory-usage").textContent = totalBytes > 0
      ? `${fmtBytes(availableBytes)} 可用 / ${fmtBytes(totalBytes)} 总计`
      : "可用量 / 总量未知";
    $("#overview-memory-fill").style.width = totalBytes > 0
      ? `${Math.max(0, Math.min(100, availableBytes / totalBytes * 100))}%`
      : "0%";
    $("#monitor-cpu-name").textContent = hw
      ? `${hw.cpuName || "处理器未知"} · ${logicalCores || "核心数未知"} 个逻辑处理器`
      : "处理器信息未知";
    updateRunEstimates();
    renderOverviewRecommendation();
    renderRecommendedModels();
    updateQuickstart();
  }

  function renderModelStorage() {
    const storage = state.storage;
    const directory = storage?.modelDirectory || state.settings?.modelDirectory || "未知";
    $("#overview-model-dir").textContent = directory;
    $("#overview-model-dir").title = directory;
    const totalBytes = Number(storage?.totalBytes);
    const availableBytes = Number(storage?.availableBytes);
    const detected = Number.isFinite(totalBytes) && totalBytes > 0 && Number.isFinite(availableBytes);
    $("#overview-disk-usage").textContent = detected
      ? `${fmtBytes(availableBytes)} 可用 / ${fmtBytes(totalBytes)} 总计`
      : "磁盘空间未知";
    $("#overview-model-disk-detail").textContent = detected
      ? `剩余 ${fmtBytes(availableBytes)} · 总容量 ${fmtBytes(totalBytes)}`
      : "未检测到模型目录所在磁盘的空间信息";
    $("#overview-disk-fill").style.width = detected
      ? `${Math.max(0, Math.min(100, availableBytes / totalBytes * 100))}%`
      : "0%";
    updateRunEstimates();
  }

  async function refreshModelStorage() {
    try {
      state.storage = await invoke("model_storage_snapshot");
    } catch {
      state.storage = null;
    }
    renderModelStorage();
  }

  function renderOverviewRecommendation() {
    const availableBytes = Number(state.hardware?.availableMemoryBytes) || 0;
    $("#overview-recommendation-copy").textContent = availableBytes > 0
      ? `本机当前检测到 ${fmtBytes(availableBytes)} 可用内存。先比较 GGUF 量化文件大小并为上下文与运行开销留余量；文件大小不等于运行内存，本机速度尚未实测。`
      : "未检测到可用内存信息，暂不能给出本机适配判断。可浏览模型，但请先核对文件大小和运行需求。";
  }

  function updateQuickstart() {
    const hardwareStep = $('[data-quick-step="hardware"]');
    if (!hardwareStep) return;
    const hardwareReady = Boolean(state.hardware);
    const chatEngine = state.engines.get("llama.cpp");
    const engineReady = Boolean(chatEngine?.installedVersions?.length);
    const modelCount = state.models.length;
    const updateStep = (name, complete, message) => {
      const step = $(`[data-quick-step="${name}"]`);
      if (!step) return;
      step.classList.toggle("complete", complete);
      const number = step.querySelector(".quick-step-number");
      number.textContent = complete ? "✓" : String(name === "hardware" ? 1 : name === "engine" ? 2 : 3);
      step.querySelector("small").textContent = message;
    };
    updateStep("hardware", hardwareReady, hardwareReady ? "本机硬件信息已读取" : "硬件信息尚未读取");
    updateStep("engine", engineReady, engineReady ? "llama.cpp 已安装" : state.enginesLoaded ? "需要安装 llama.cpp 推理引擎" : "正在读取引擎安装状态…");
    updateStep("models", modelCount > 0, modelCount ? `已找到 ${modelCount} 个本地 GGUF 模型` : "导入 GGUF 或从模型库下载");
  }
  async function loadHardware() {
    const refreshButton = $("#refresh-hardware");
    if (refreshButton) { refreshButton.disabled = true; refreshButton.textContent = "正在检测…"; }
    try {
      state.hardware = await invoke("hardware_snapshot");
      renderOverviewHardware();
      if (state.hardware && state.recommendationCategory === "fit" && document.querySelector('[data-recommend-category="fit"]')) {
        state.recommendationsLoaded = false;
        if (document.querySelector("#page-models.active") && !$("#models-recommended").hidden) searchRecommendationModels("fit");
      }
    } catch (error) {
      state.hardware = null;
      renderOverviewHardware();
      $("#overview-copy").textContent = `硬件检测失败：${error}。未取得的数据会显示为未知。`;
      showToast(`硬件检测失败：${error}`, "error");
    } finally {
      if (refreshButton) { refreshButton.disabled = false; refreshButton.textContent = "重新检测硬件"; }
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
      state.enginesLoaded = true;
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
          button.textContent = "此平台暂不支持安装";
        });
      }
      renderMediaEngineBanners();
      renderMediaLibraryEngine();
      updateQuickstart();
    } catch (error) {
      state.enginesLoaded = true;
      [$("#engine-cards"), $("#overview-engines")].forEach((box) => { box.innerHTML = `<div class="error-box">无法读取引擎状态：${escapeText(error)}</div>`; });
      renderMediaEngineBanners(String(error));
      renderMediaLibraryEngine(String(error));
      updateQuickstart();
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
        description.textContent = "stable-diffusion.cpp 官方 sd-cli 已安装，可尝试真实图像与 Wan 视频推理。当前安装包为 CPU x64，不调用 GPU；模型及配套组件需另行下载或从 ComfyUI 只读选择。";
        install.hidden = true;
        manage.hidden = false;
      } else {
        status.className = "status-pill empty";
        status.textContent = known ? "尚未安装" : "状态未知";
        description.textContent = "画图和视频共用 stable-diffusion.cpp 官方 CPU x64 引擎；安装不会下载模型权重。GPU 版本未接入此安装器。";
        install.hidden = false;
        manage.hidden = true;
        install.disabled = !known || !state.mediaInstallSupported || state.mediaInstallBusy;
        install.textContent = !known ? "无法读取引擎状态" : (!state.mediaInstallSupported ? "此平台暂不支持安装" : (state.mediaInstallBusy ? "正在安装…" : "一键安装"));
      }
    });
  }

  function renderMediaLibraryEngine(loadError = "") {
    const status = $("#media-engine-status");
    const latestBox = $("#media-engine-latest");
    if (!status || !latestBox) return;
    if (loadError) { status.textContent = `无法读取引擎状态：${loadError}`; return; }
    const versions = state.engines.get("stable-diffusion.cpp")?.installedVersions || [];
    status.textContent = versions.length
      ? `已安装 CPU x64 版本：${versions.map((version) => version.tag).join("、")}；GPU 不会被此构建使用。`
      : "尚未安装 stable-diffusion.cpp；引擎检查不会自动安装或下载模型权重。";
  }

  function renderComfyUiDirectoryState() {
    const directory = state.settings?.comfyuiDirectory;
    $("#comfyui-directory").textContent = directory || "尚未选择文件夹";
    $("#comfyui-directory").title = directory || "";
    $("#scan-comfyui-directory").disabled = !directory;
    if (!directory && !state.comfyuiScan) $("#comfyui-scan-status").textContent = "未扫描";
  }

  function renderComfyUiModels(scan) {
    const box = $("#comfyui-model-list");
    box.replaceChildren();
    if (!scan.files.length) {
      const empty = document.createElement("div"); empty.className = "empty-state";
      empty.textContent = "扫描完成，没有在常见 ComfyUI 模型目录中发现已识别的权重文件。";
      box.append(empty); return;
    }
    const categories = new Map([
      ["checkpoints", "主模型 / Checkpoints"], ["diffusion_models", "Diffusion 模型"], ["unet", "UNet"],
      ["loras", "LoRA"], ["vae", "VAE"], ["text_encoders", "文本编码器"], ["clip", "CLIP"],
      ["clip_vision", "CLIP Vision"], ["controlnet", "ControlNet"], ["embeddings", "Embeddings"],
      ["upscale_models", "放大模型"], ["style_models", "风格模型"], ["hypernetworks", "Hypernetwork"],
      ["animatediff_models", "AnimateDiff"],
    ]);
    scan.files.forEach((model) => {
      const row = document.createElement("article"); row.className = "comfyui-model-row";
      const info = document.createElement("div"); info.className = "comfyui-model-info";
      const title = document.createElement("strong"); title.textContent = model.name; title.title = model.name;
      const kind = document.createElement("small"); kind.textContent = categories.get(model.category) || model.category;
      const path = document.createElement("code"); path.textContent = model.path; path.title = model.path;
      info.append(title, kind, path);
      const size = document.createElement("strong"); size.className = "comfyui-model-size"; size.textContent = fmtBytes(Number(model.sizeBytes));
      row.append(info, size); box.append(row);
    });
    if (scan.truncated) {
      const note = document.createElement("p"); note.className = "muted tiny";
      note.textContent = "达到安全扫描预算（最多 50,000 条目、5,000 个目录、5,000 个权重文件 / 6 层）；清单可能不完整。";
      box.append(note);
    }
  }

  async function scanComfyUiDirectory(directory) {
    if (!directory) return;
    state.comfyuiScan = null; state.comfyuiScanPath = directory; state.comfyuiScanError = "";
    $("#comfyui-scan-status").textContent = "正在只读扫描常见模型目录…";
    $("#comfyui-model-list").innerHTML = `<div class="empty-state">正在检查文件夹；不会打开模型内容或修改文件。</div>`;
    try {
      const scan = await invoke("scan_comfyui_models", { directory });
      state.comfyuiScan = scan;
      $("#comfyui-scan-status").textContent = `${scan.files.length} 个权重文件 · 扫描位置：${scan.modelsDirectory}`;
      renderComfyUiModels(scan);
      populateMediaModelSelectors();
    } catch (error) {
      state.comfyuiScanError = String(error);
      $("#comfyui-scan-status").textContent = `扫描失败：${error}`;
      $("#comfyui-model-list").innerHTML = `<div class="error-box">无法读取该目录。可确认文件夹存在并重试。</div>`;
      populateMediaModelSelectors();
    }
  }

  function mediaInventory() {
    const managed = (state.mediaModels || []).map((item) => ({ ...item, source: "LocModel 媒体目录" }));
    const imported = (state.comfyuiScan?.files || [])
      .filter((item) => /\.(safetensors|gguf)$/i.test(item.name || item.path || ""))
      .map((item) => ({ ...item, format: String(item.path || item.name).split(".").pop().toLowerCase(), source: `ComfyUI · ${item.category}` }));
    const unique = new Map();
    [...managed, ...imported].forEach((item) => { if (item.path) unique.set(item.path, item); });
    return [...unique.values()].sort((left, right) => String(left.name).localeCompare(String(right.name)));
  }

  function fillMediaSelect(select, emptyLabel, keepValue = true) {
    if (!select) return;
    const previous = keepValue ? select.value : "";
    select.replaceChildren(new Option(emptyLabel, ""));
    mediaInventory().forEach((model) => {
      const option = new Option(`${model.name} · ${model.source} · ${fmtBytes(Number(model.sizeBytes))}`, model.path);
      option.title = model.path;
      select.add(option);
    });
    if (previous && [...select.options].some((option) => option.value === previous)) select.value = previous;
  }

  function populateMediaModelSelectors() {
    fillMediaSelect($("#image-model-select"), "选择主模型…");
    fillMediaSelect($("#image-vae-select"), "不指定额外 VAE");
    fillMediaSelect($("#image-text-select"), "不指定额外编码器");
    fillMediaSelect($("#video-model-select"), "选择 Wan 主模型 / 低噪声模型…");
    fillMediaSelect($("#video-high-noise-select"), "无 / 单专家模型");
    fillMediaSelect($("#video-vae-select"), "选择 Wan VAE…");
    fillMediaSelect($("#video-text-select"), "选择 T5-XXL 文本编码器…");
    updateMediaResourceStatus("image");
    updateMediaResourceStatus("video");
  }

  function renderManagedMediaModels() {
    const box = $("#managed-media-model-list");
    if (!box) return;
    box.replaceChildren();
    if (!state.mediaModels.length) {
      const empty = document.createElement("div"); empty.className = "empty-state";
      empty.textContent = "还没有通过 Hugging Face 下载的媒体组件。文件只会在你明确选择后下载。";
      box.append(empty); return;
    }
    state.mediaModels.forEach((model) => {
      const row = document.createElement("article"); row.className = "comfyui-model-row";
      const info = document.createElement("div"); info.className = "comfyui-model-info";
      const title = document.createElement("strong"); title.textContent = model.name;
      const path = document.createElement("code"); path.textContent = model.path; path.title = model.path;
      info.append(title, path);
      const size = document.createElement("strong"); size.className = "comfyui-model-size"; size.textContent = `${fmtBytes(Number(model.sizeBytes))} · ${String(model.format || "").toUpperCase()}`;
      row.append(info, size); box.append(row);
    });
  }

  async function refreshMediaModels() {
    try {
      state.mediaModels = await invoke("list_media_models");
      renderManagedMediaModels();
      populateMediaModelSelectors();
    } catch (error) {
      const box = $("#managed-media-model-list");
      if (box) box.textContent = `读取媒体模型目录失败：${error}`;
    }
  }

  function selectedMediaSize(selectors) {
    const paths = selectors.map((selector) => $(selector)?.value).filter(Boolean);
    const inventory = new Map(mediaInventory().map((model) => [model.path, Number(model.sizeBytes) || 0]));
    return paths.reduce((sum, path) => sum + (inventory.get(path) || 0), 0);
  }

  function updateMediaResourceStatus(kind) {
    const isVideo = kind === "video";
    const box = $(`#${kind}-generation-status`);
    if (!box) return;
    const selectedBytes = selectedMediaSize(isVideo
      ? ["#video-model-select", "#video-high-noise-select", "#video-vae-select", "#video-text-select"]
      : ["#image-model-select", "#image-vae-select", "#image-text-select"]);
    const available = Number(state.hardware?.availableMemoryBytes);
    const ramNote = Number.isFinite(available) && available > 0 ? ` · 当前可用系统内存 ${fmtBytes(available)}` : "";
    const selectedNote = selectedBytes ? `已选组件文件总大小 ${fmtBytes(selectedBytes)}${ramNote}；` : "";
    box.textContent = isVideo
      ? `${selectedNote}文件大小不等于运行内存。当前 CPU 引擎不使用 GPU；Wan 33 帧任务可能需要大量资源，实际要求取决于权重和后端，不能保证可完成。`
      : `${selectedNote}文件大小不等于运行内存。当前 CPU 引擎不使用 GPU；能否运行取决于模型所需组件与系统资源，速度不作承诺。`;
  }

  async function chooseComfyUiDirectory() {
    const button = $("#choose-comfyui-directory"); button.disabled = true;
    try {
      const directory = await invoke("choose_directory");
      if (!directory) return;
      const current = state.settings || await invoke("app_settings");
      state.settings = await invoke("save_app_settings", { settings: { ...current, comfyuiDirectory: directory } });
      renderComfyUiDirectoryState();
      await scanComfyUiDirectory(state.settings.comfyuiDirectory);
    } catch (error) { $("#comfyui-scan-status").textContent = `保存或扫描目录失败：${error}`; }
    finally { button.disabled = false; }
  }

  async function checkMediaEngineVersion() {
    const button = $("#media-engine-check");
    const latestBox = $("#media-engine-latest");
    button.disabled = true; latestBox.textContent = "正在查询 stable-diffusion.cpp 官方版本…";
    try {
      const latest = await invoke("engine_latest", { engine: "media" });
      const installed = state.engines.get("stable-diffusion.cpp")?.installedVersions || [];
      const upToDate = installed.some((version) => version.tag === latest.tag);
      latestBox.textContent = upToDate
        ? `官方最新版本 ${latest.tag} · 本机已安装；如需更新请前往“版本管理”。`
        : `官方最新版本：${latest.tag} · 本机版本：${installed.map((version) => version.tag).join("、") || "未安装"} · 不会自动安装。`;
    } catch (error) { latestBox.textContent = `版本查询失败：${error}`; }
    finally { button.disabled = false; }
  }

  async function installMediaEngine() {
    if (state.mediaInstallBusy) return;
    if (!state.mediaInstallSupported) { showToast("此平台暂不支持自动安装引擎", "error"); return; }
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
      updateQuickstart();
    } catch (error) { $("#models-installed").innerHTML = `<div class="error-box">无法读取模型列表：${escapeText(error)}</div>`; updateQuickstart(); }
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

  const recommendationCategoryConfig = {
    fit: { label: "适合我的电脑", query: "GGUF", memoryFilter: true },
    all: { label: "全部", query: "GGUF" },
    chat: { label: "对话", query: "instruct" },
    code: { label: "编程", query: "code" },
    vision: { label: "看图", query: "vision" },
    reasoning: { label: "推理", query: "reasoning" },
  };

  function renderRecommendedModels() {
    const availableBytes = Number(state.hardware?.availableMemoryBytes) || 0;
    $("#recommendation-note").textContent = availableBytes > 0
      ? `已检测到 ${fmtBytes(availableBytes)} 可用内存。分类按关键词检索 Hugging Face GGUF 仓库；“适合我的电脑”只筛选单文件大小不超过当前可用内存的候选，不保证模型兼容或可运行，模型文件大小也不等于实际内存需求。`
      : "未检测到可用内存；不会显示本机适配建议。其余分类仍可搜索 Hugging Face GGUF 仓库，性能数据不作估算。";
    $$("[data-recommend-category]").forEach((button) => {
      const active = button.dataset.recommendCategory === state.recommendationCategory;
      button.classList.toggle("active", active);
      button.setAttribute("aria-pressed", String(active));
    });
  }

  function isShardedGguf(fileName) {
    return /(?:-|_|\.)\d{1,6}-of-\d{1,6}\.gguf$/i.test(fileName);
  }

  function renderRecommendationCards(items, category) {
    const box = $("#recommendation-results");
    box.replaceChildren();
    if (!items.length) {
      const empty = document.createElement("div"); empty.className = "empty-state";
      empty.textContent = category === "fit"
        ? "在已检查的仓库中，没有找到大小不超过当前可用内存、且包含 SHA-256 的单文件 GGUF。可改选“全部”，查看文件后再判断；这不是兼容性结论。"
        : `没有找到“${recommendationCategoryConfig[category].label}”相关结果；可改用 Hugging Face 模型库搜索。`;
      box.append(empty);
      return;
    }
    items.forEach(({ model, files = null }) => {
      const card = document.createElement("article"); card.className = "hf-model-card recommendation-model-card";
      const details = document.createElement("div"); details.className = "hf-model-summary";
      const repo = document.createElement("strong"); repo.textContent = model.repoId;
      const meta = document.createElement("small");
      meta.textContent = `${Number(model.downloads || 0).toLocaleString()} 次下载 · ${Number(model.likes || 0).toLocaleString()} 个赞 · ${model.pipelineTag || "GGUF"}`;
      details.append(repo, meta);
      if (category === "fit") {
        const fitNote = document.createElement("small"); fitNote.className = "fit-file-note";
        fitNote.textContent = `找到 ${files.length} 个候选文件，按本机可用内存 ${fmtBytes(Number(state.hardware?.availableMemoryBytes))} 预筛`;
        details.append(fitNote);
      }
      const actions = document.createElement("div"); actions.className = "hf-model-actions";
      const link = document.createElement("a"); link.className = "button secondary";
      link.href = `https://huggingface.co/${model.repoId.split("/").map(encodeURIComponent).join("/")}`;
      link.target = "_blank"; link.rel = "noopener noreferrer"; link.textContent = "查看模型卡 ↗";
      const choose = document.createElement("button"); choose.className = "button secondary";
      choose.textContent = category === "fit" ? "选择量化并下载" : "查看量化文件";
      const fileBox = document.createElement("div"); fileBox.className = "hf-files recommendation-files"; fileBox.hidden = true;
      choose.addEventListener("click", async () => {
        fileBox.hidden = false;
        await loadHfFiles(model.repoId, fileBox, files, category === "fit" ? Number(state.hardware?.availableMemoryBytes) : null);
      });
      actions.append(link, choose);
      card.append(details, actions, fileBox); box.append(card);
    });
  }

  async function searchRecommendationModels(category = state.recommendationCategory) {
    const config = recommendationCategoryConfig[category];
    if (!config) return;
    state.recommendationCategory = category;
    const requestId = ++state.recommendationSeq;
    state.recommendationsLoaded = false;
    renderRecommendedModels();
    const box = $("#recommendation-results");
    const availableBytes = Number(state.hardware?.availableMemoryBytes) || 0;
    if (config.memoryFilter && availableBytes <= 0) {
      box.innerHTML = `<div class="empty-state">未检测到当前可用内存，无法进行“适合我的电脑”预筛。可选择“全部”浏览真实 GGUF 仓库。</div>`;
      return;
    }
    box.innerHTML = `<div class="empty-state">正在查询 Hugging Face 的 ${escapeText(config.label)} GGUF 仓库…不会下载权重。</div>`;
    try {
      const models = await invoke("search_huggingface", { query: config.query, category: "chat" });
      if (requestId !== state.recommendationSeq) return;
      if (!config.memoryFilter) {
        renderRecommendationCards(models.map((model) => ({ model })), category);
        state.recommendationsLoaded = true;
        return;
      }
      const candidates = [];
      const repos = models.slice(0, 8);
      for (let index = 0; index < repos.length; index += 1) {
        if (requestId !== state.recommendationSeq) return;
        $("#recommendation-results").innerHTML = `<div class="empty-state">正在核对 ${index + 1}/${repos.length} 个仓库的真实文件大小…</div>`;
        try {
          const files = await invoke("huggingface_files", { repoId: repos[index].repoId });
          const eligible = files.filter((file) => Number.isFinite(Number(file.sizeBytes)) && Number(file.sizeBytes) > 0
            && Number(file.sizeBytes) <= availableBytes && typeof file.sha256 === "string"
            && /^[0-9a-f]{64}$/i.test(file.sha256) && !isShardedGguf(file.name));
          if (eligible.length) candidates.push({ model: repos[index], files: eligible });
        } catch { /* Skip repositories whose file metadata is unavailable; no size is guessed. */ }
      }
      if (requestId !== state.recommendationSeq) return;
      renderRecommendationCards(candidates, category);
      state.recommendationsLoaded = true;
    } catch (error) {
      if (requestId !== state.recommendationSeq) return;
      const failure = document.createElement("div"); failure.className = "error-box";
      const message = document.createElement("p"); message.textContent = `推荐查询失败：${error}`;
      const retry = document.createElement("button"); retry.className = "button secondary"; retry.textContent = "重试";
      retry.addEventListener("click", () => searchRecommendationModels(category));
      failure.append(message, retry); box.replaceChildren(failure);
      state.recommendationsLoaded = true;
    }
  }

  const hfCategoryConfig = {
    all: { label: "全部模型", query: "GGUF", note: "按关键词搜索公开模型仓库，不限定任务标签；单文件 GGUF 可在核对元数据后下载，其他格式只查看仓库信息。" },
    chat: { label: "对话 · GGUF", query: "GGUF", note: "对话分类通过 GGUF 仓库标签筛选；仓库下载量和点赞数来自 Hugging Face。" },
    image: { label: "画图 · 文生图", query: "diffusion", note: "画图分类按 Hugging Face 的 text-to-image 任务标签筛选。可查看仓库中的 Safetensors / GGUF 组件，手动勾选需要的文件成组下载。" },
    video: { label: "视频 · 文生视频", query: "video", note: "视频分类按 Hugging Face 的 text-to-video 任务标签筛选。可查看并勾选 Wan 等模型所需的多份 Safetensors / GGUF 组件。" },
  };

  function setHfCategory(category, resetQuery = false) {
    const config = hfCategoryConfig[category];
    if (!config) return false;
    const previousQuery = hfCategoryConfig[state.hfCategory]?.query;
    const input = $("#model-search-query");
    const shouldUseDefault = resetQuery || !input.value.trim() || input.value.trim() === previousQuery;
    state.hfCategory = category;
    state.hfSearchSeq += 1;
    if (shouldUseDefault) input.value = config.query;
    input.placeholder = `搜索 ${config.label} 仓库`;
    $("#hf-library-note").textContent = config.note;
    $("#hf-library-disclosure").textContent = category === "chat" || category === "all"
      ? "查询会访问 huggingface.co。单文件 GGUF 下载会在开始前核对仓库大小、LFS SHA-256 和模型盘剩余空间；下载有进度与取消。分片 GGUF 暂不支持成组下载。"
      : "只在你手动勾选并点击下载后获取权重；每个组件固定到仓库当前 commit、逐件校验大小与 LFS SHA-256、检查总磁盘空间。仅接受 .safetensors / .gguf，不下载或运行 Python 脚本、配置文件或 pickle 权重。";
    $("#hf-files").hidden = true;
    $("#hf-files").replaceChildren();
    $$("[data-hf-category]").forEach((button) => {
      const active = button.dataset.hfCategory === category;
      button.classList.toggle("active", active);
      button.setAttribute("aria-pressed", String(active));
    });
    return true;
  }

  function openHuggingFaceLibrary(category = "all", query = null) {
    if (!setHfCategory(category, query !== null)) return;
    if (query !== null) $("#model-search-query").value = query;
    switchModelTab("download");
    location.hash = "#models";
    setPage("models");
    $("#model-search-form").requestSubmit();
  }

  function prepareRecommendedMedia(category, repoId, fileNames) {
    if (!setHfCategory(category, true)) return;
    $("#model-search-query").value = repoId;
    $("#hf-results").replaceChildren();
    $("#hf-result-summary").textContent = "已打开 stable-diffusion.cpp 官方指南列出的 Hugging Face 仓库；推荐组件只预选，不会自动下载。";
    switchModelTab("download");
    location.hash = "#models";
    setPage("models");
    loadHfMediaFiles(repoId, fileNames);
  }

  function renderHfModels(models, category) {
    const box = $("#hf-results"); box.replaceChildren();
    if (!models.length) {
      const empty = document.createElement("div"); empty.className = "empty-state";
      empty.textContent = `没有找到${hfCategoryConfig[category].label}结果；调整关键词后可重试。`;
      box.append(empty);
      return;
    }
    models.forEach((model) => {
      const card = document.createElement("article"); card.className = "hf-model-card";
      const details = document.createElement("div");
      const repo = document.createElement("strong"); repo.textContent = model.repoId;
      const meta = document.createElement("small");
      const task = model.pipelineTag || (category === "chat" ? "GGUF" : "任务标签未提供");
      meta.textContent = `${Number(model.downloads || 0).toLocaleString()} 次下载 · ${Number(model.likes || 0).toLocaleString()} 个赞 · ${task}`;
      details.append(repo, meta);
      const actions = document.createElement("div"); actions.className = "hf-model-actions";
      const link = document.createElement("a"); link.className = "button secondary";
      link.href = `https://huggingface.co/${model.repoId.split("/").map(encodeURIComponent).join("/")}`;
      link.target = "_blank"; link.rel = "noopener noreferrer"; link.textContent = "打开 Hugging Face 仓库 ↗";
      actions.append(link);
      if (category === "chat" || category === "all") {
        const button = document.createElement("button"); button.className = "button secondary"; button.textContent = "查看 GGUF 文件";
        button.addEventListener("click", () => loadHfFiles(model.repoId));
        actions.append(button);
      }
      if (category === "image" || category === "video") {
        const button = document.createElement("button"); button.className = "button secondary"; button.textContent = "查看组件并选择下载";
        button.addEventListener("click", () => loadHfMediaFiles(model.repoId));
        actions.append(button);
      }
      card.append(details, actions); box.append(card);
    });
  }

  async function loadHfFiles(repoId, box = $("#hf-files"), prefetchedFiles = null, maxSizeBytes = null) {
    box.hidden = false; box.replaceChildren();
    const heading = document.createElement("h3"); heading.textContent = `${repoId} · GGUF 文件`;
    const loading = document.createElement("p"); loading.className = "muted"; loading.textContent = "正在读取文件列表…"; box.append(heading, loading);
    try {
      const files = Array.isArray(prefetchedFiles) ? prefetchedFiles : await invoke("huggingface_files", { repoId });
      if (!box.isConnected) return;
      box.replaceChildren(heading);
      if (!files.length) { const empty = document.createElement("p"); empty.textContent = "此仓库没有可识别的 GGUF 文件。"; box.append(empty); return; }
      const select = document.createElement("select");
      select.setAttribute("aria-label", "选择 GGUF 模型文件");
      files.forEach((file, index) => {
        const quantization = file.quantization || "量化未标注";
        const size = Number(file.sizeBytes) > 0 ? fmtBytes(Number(file.sizeBytes)) : "大小未知";
        select.add(new Option(`${quantization} · ${file.name} · ${size}`, String(index)));
      });
      const actions = document.createElement("div"); actions.className = "hf-file-actions";
      const download = document.createElement("button"); download.className = "button primary"; download.textContent = "开始下载";
      const cancel = document.createElement("button"); cancel.className = "button danger"; cancel.textContent = "取消下载"; cancel.hidden = true;
      const refreshSpace = document.createElement("button"); refreshSpace.className = "button secondary"; refreshSpace.textContent = "刷新磁盘空间";
      const fileInfo = document.createElement("p"); fileInfo.className = "muted tiny hf-file-info";
      const progress = document.createElement("div"); progress.className = "download-progress"; progress.hidden = true;
      const disclosure = document.createElement("p"); disclosure.className = "muted tiny";
      disclosure.textContent = "仅在你点击“开始下载”后下载。应用先核对仓库文件大小、SHA-256 与模型盘空间；完成后校验哈希。分片文件不提供单文件下载，需在 Hugging Face 获取完整分片集合。";
      const downloadBox = document.createElement("div"); downloadBox.className = "download-control-row";
      const safeFiles = maxSizeBytes === null ? files : files.filter((file) => Number(file.sizeBytes) <= maxSizeBytes);
      if (safeFiles.length && maxSizeBytes !== null) {
        files.splice(0, files.length, ...safeFiles);
        select.replaceChildren();
        files.forEach((file, index) => select.add(new Option(`${file.quantization || "量化未标注"} · ${file.name} · ${fmtBytes(Number(file.sizeBytes))}`, String(index))));
      }
      function updateSelectedFile() {
        const file = files[Number(select.value)];
        if (!file) { download.disabled = true; fileInfo.textContent = "没有符合当前筛选的 GGUF 文件。"; return; }
        const size = Number(file.sizeBytes);
        const available = Number(state.storage?.availableBytes);
        const hasSpaceData = Number.isFinite(available) && available >= 0;
        const requiredSpace = size + 256 * 1024 * 1024;
        const enoughSpace = hasSpaceData && Number.isFinite(requiredSpace) && requiredSpace <= available;
        const sharded = isShardedGguf(file.name);
        const hasHash = typeof file.sha256 === "string" && /^[0-9a-f]{64}$/i.test(file.sha256);
        const busy = Boolean(state.currentDownloadId);
        download.disabled = busy || !Number.isFinite(size) || size <= 0 || !hasHash || !enoughSpace || sharded;
        download.textContent = size > 0 ? `下载 ${fmtBytes(size)}` : "无法核对文件大小";
        const license = file.license || "模型卡未声明许可证";
        fileInfo.textContent = `量化：${file.quantization || "文件名未标注"} · 文件：${size > 0 ? fmtBytes(size) : "大小未知"} · 许可证：${license}`;
        if (sharded) fileInfo.textContent += " · 多分片文件组暂不支持成组下载";
        else if (!hasHash) fileInfo.textContent += " · 仓库未提供可核实 SHA-256，已禁用应用内下载";
        else if (!hasSpaceData) fileInfo.textContent += " · 目标磁盘空间未知，先刷新后才能下载";
        else if (!enoughSpace) fileInfo.textContent += ` · 需预留文件大小 + 256 MiB，剩余 ${fmtBytes(available)}`;
      }
      select.addEventListener("change", updateSelectedFile);
      refreshSpace.addEventListener("click", async () => { refreshSpace.disabled = true; await refreshModelStorage(); updateSelectedFile(); refreshSpace.disabled = false; });
      download.addEventListener("click", async () => {
        if (state.currentDownloadId) return;
        download.disabled = true; refreshSpace.disabled = true; progress.hidden = false; progress.textContent = "正在重新检查模型盘剩余空间…";
        await refreshModelStorage();
        const file = files[Number(select.value)];
        const available = Number(state.storage?.availableBytes);
        const size = Number(file?.sizeBytes);
        if (!file || !Number.isFinite(available) || !Number.isFinite(size) || size <= 0 || size + 256 * 1024 * 1024 > available) {
          progress.textContent = "空间不足（需额外预留 256 MiB）或磁盘空间未知；未开始下载。"; refreshSpace.disabled = false; updateSelectedFile(); return;
        }
        const downloadId = globalThis.crypto?.randomUUID?.() || `download-${Date.now()}-${Math.random().toString(16).slice(2)}`;
        state.currentDownloadId = downloadId;
        progress.dataset.downloadId = downloadId;
        cancel.hidden = false; cancel.disabled = false; cancel.textContent = "取消下载";
        select.disabled = true; progress.textContent = "正在连接 Hugging Face…";
        try {
          await invoke("download_huggingface_model", { repoId, fileName: file.name, downloadId });
          await Promise.all([loadModels(), refreshModelStorage()]); progress.textContent = "下载完成，SHA-256 已核验并保存到模型目录。"; showToast("模型下载完成并已校验");
        } catch (error) {
          const rawError = String(error);
          const cancelled = rawError.startsWith("E_DOWNLOAD_CANCELLED:");
          const message = cancelled ? rawError.slice("E_DOWNLOAD_CANCELLED:".length) : rawError;
          progress.textContent = cancelled
            ? (message.includes("临时文件清理失败") ? `下载已取消；${message}` : "下载已取消，临时文件已清理。")
            : `下载失败：${message}`;
        } finally {
          state.currentDownloadId = null; cancel.hidden = true; select.disabled = false; refreshSpace.disabled = false; updateSelectedFile();
        }
      });
      cancel.addEventListener("click", async () => {
        if (!state.currentDownloadId) return;
        cancel.disabled = true; cancel.textContent = "正在取消…";
        try { await invoke("cancel_huggingface_download", { downloadId: state.currentDownloadId }); }
        catch (error) { progress.textContent = `取消请求失败：${error}`; }
      });
      downloadBox.append(download, cancel, refreshSpace);
      actions.append(downloadBox);
      box.append(select, fileInfo, actions, disclosure, progress);
      updateSelectedFile();
    } catch (error) {
      loading.textContent = `读取文件失败：${error}`;
      const retry = document.createElement("button"); retry.className = "button secondary"; retry.textContent = "重试读取文件";
      retry.addEventListener("click", () => loadHfFiles(repoId, box, null, maxSizeBytes)); box.append(retry);
    }
  }

  async function loadHfMediaFiles(repoId, recommendedFiles = []) {
    const box = $("#hf-files"); box.hidden = false; box.replaceChildren();
    const heading = document.createElement("h3"); heading.textContent = `${repoId} · 媒体权重组件`;
    const loading = document.createElement("p"); loading.className = "muted"; loading.textContent = "读取固定版本文件清单…"; box.append(heading, loading);
    try {
      const files = await invoke("huggingface_media_files", { repoId });
      if (!box.isConnected) return;
      box.replaceChildren(heading);
      if (!files.length) { const empty = document.createElement("p"); empty.textContent = "此仓库没有可安全下载的 .safetensors / .gguf 权重文件。"; box.append(empty); return; }
      const list = document.createElement("div"); list.className = "media-remote-file-list";
      const rows = files.map((file) => {
        const size = Number(file.sizeBytes);
        const hashOk = typeof file.sha256 === "string" && /^[0-9a-f]{64}$/i.test(file.sha256);
        const sizeOk = Number.isFinite(size) && size > 0 && size <= 150 * 1024 ** 3;
        const row = document.createElement("label"); row.className = "media-remote-file-row";
        const checkbox = document.createElement("input"); checkbox.type = "checkbox"; checkbox.value = file.name; checkbox.disabled = !hashOk || !sizeOk;
        checkbox.checked = recommendedFiles.includes(file.name) && !checkbox.disabled;
        const detail = document.createElement("span"); detail.className = "media-remote-file-detail";
        const name = document.createElement("strong"); name.textContent = file.name;
        const meta = document.createElement("small"); meta.textContent = `${sizeOk ? fmtBytes(size) : "大小未知 / 超过 150 GiB"} · SHA-256 ${hashOk ? "可用" : "缺失"} · 许可证：${file.license || "模型卡未声明"}`;
        detail.append(name, meta); row.append(checkbox, detail); list.append(row);
        return { file, checkbox, sizeOk, hashOk };
      });
      const summary = document.createElement("p"); summary.className = "muted tiny";
      if (recommendedFiles.length) {
        const note = document.createElement("p"); note.className = "muted tiny";
        const selectedRecommendations = rows.filter((row) => row.checkbox.checked).length;
        note.textContent = selectedRecommendations === recommendedFiles.length
          ? "官方指南指定组件已预选；请先核对文件大小、哈希和许可证，再点击下载。"
          : `上游仓库清单中找到 ${selectedRecommendations}/${recommendedFiles.length} 个指定组件；仓库可能更新，请先检查下方结果。未自动下载。`;
        box.append(note);
      }
      const actions = document.createElement("div"); actions.className = "media-remote-actions";
      const download = document.createElement("button"); download.className = "button primary"; download.textContent = "下载所选组件";
      const cancel = document.createElement("button"); cancel.className = "button danger"; cancel.textContent = "取消下载"; cancel.hidden = true;
      const refreshSpace = document.createElement("button"); refreshSpace.className = "button secondary"; refreshSpace.textContent = "刷新磁盘空间";
      const progress = document.createElement("div"); progress.className = "download-progress"; progress.hidden = true;
      const selection = () => {
        const chosen = rows.filter((row) => row.checkbox.checked);
        const bytes = chosen.reduce((sum, row) => sum + Number(row.file.sizeBytes || 0), 0);
        const available = Number(state.storage?.availableBytes);
        const known = Number.isFinite(available) && available >= 0;
        const fits = known && bytes + 512 * 1024 * 1024 <= available && bytes <= 200 * 1024 ** 3;
        summary.textContent = chosen.length
          ? `${chosen.length} 个组件 · ${fmtBytes(bytes)} · 额外预留 512 MiB · ${known ? `可用 ${fmtBytes(available)}` : "磁盘空间未知"}`
          : "按模型卡手动选择一组配套权重；配置文件、tokenizer、脚本和其他格式不会下载。";
        if (chosen.length && !fits) summary.textContent += bytes > 200 * 1024 ** 3 ? " · 超过 200 GiB 组上限" : " · 磁盘空间不足或未知";
        download.disabled = Boolean(state.currentDownloadId) || !chosen.length || !fits || chosen.some((row) => !row.sizeOk || !row.hashOk);
        download.textContent = chosen.length ? `下载 ${chosen.length} 个组件（${fmtBytes(bytes)}）` : "下载所选组件";
      };
      rows.forEach((row) => row.checkbox.addEventListener("change", selection));
      refreshSpace.addEventListener("click", async () => { refreshSpace.disabled = true; await refreshModelStorage(); refreshSpace.disabled = false; selection(); });
      download.addEventListener("click", async () => {
        const chosen = rows.filter((row) => row.checkbox.checked);
        if (!chosen.length || state.currentDownloadId) return;
        download.disabled = true; refreshSpace.disabled = true; progress.hidden = false; progress.textContent = "重新检查磁盘空间…";
        await refreshModelStorage();
        const bytes = chosen.reduce((sum, row) => sum + Number(row.file.sizeBytes || 0), 0);
        const available = Number(state.storage?.availableBytes);
        if (!Number.isFinite(available) || bytes + 512 * 1024 * 1024 > available) {
          progress.textContent = "空间不足或无法确认磁盘可用空间；未开始下载。"; refreshSpace.disabled = false; selection(); return;
        }
        const downloadId = globalThis.crypto?.randomUUID?.() || `media-${Date.now()}-${Math.random().toString(16).slice(2)}`;
        state.currentDownloadId = downloadId; progress.dataset.downloadId = downloadId;
        cancel.hidden = false; cancel.disabled = false; cancel.textContent = "取消下载";
        rows.forEach((row) => { row.checkbox.disabled = true; }); progress.textContent = "正在下载并核验媒体组件…";
        try {
          await invoke("download_media_model_bundle", { repoId, fileNames: chosen.map((row) => row.file.name), downloadId });
          await Promise.all([refreshMediaModels(), refreshModelStorage()]);
          progress.textContent = "组件均已通过 SHA-256 校验并保存；配套关系和许可证请再核对模型卡。"; showToast("媒体组件下载并校验完成");
        } catch (error) {
          const raw = String(error); const cancelled = raw.startsWith("E_DOWNLOAD_CANCELLED:");
          progress.textContent = `${cancelled ? "下载已取消：" : "下载失败："}${cancelled ? raw.slice("E_DOWNLOAD_CANCELLED:".length) : raw}`;
        } finally {
          state.currentDownloadId = null; cancel.hidden = true; refreshSpace.disabled = false;
          rows.forEach((row) => { row.checkbox.disabled = !row.hashOk || !row.sizeOk; }); selection();
        }
      });
      cancel.addEventListener("click", async () => {
        if (!state.currentDownloadId) return;
        cancel.disabled = true; cancel.textContent = "正在取消…";
        try { await invoke("cancel_huggingface_download", { downloadId: state.currentDownloadId }); }
        catch (error) { progress.textContent = `取消请求失败：${error}`; }
      });
      actions.append(download, cancel, refreshSpace);
      const disclosure = document.createElement("p"); disclosure.className = "muted tiny";
      disclosure.textContent = "仅下载你明确勾选的权重文件。使用仓库当前 commit 固定版本，逐件核对大小与 LFS SHA-256；失败时已校验组件保留，取消时清理未完成临时文件。下载不代表组件兼容。";
      box.append(list, summary, actions, disclosure, progress); selection();
      await refreshModelStorage(); selection();
    } catch (error) {
      loading.textContent = `读取组件清单失败：${error}`;
      const retry = document.createElement("button"); retry.className = "button secondary"; retry.textContent = "重试读取组件";
      retry.addEventListener("click", () => loadHfMediaFiles(repoId)); box.append(retry);
    }
  }

  async function searchHf(event) {
    event.preventDefault();
    const category = state.hfCategory;
    const requestId = ++state.hfSearchSeq;
    const button = $("#model-search-form button[type=submit]");
    button.disabled = true; button.textContent = "搜索中…";
    $("#hf-files").hidden = true;
    $("#hf-result-summary").textContent = `正在查询 ${hfCategoryConfig[category].label}；请求最多返回 20 项并按下载量排序。`;
    const loading = document.createElement("div"); loading.className = "empty-state"; loading.textContent = "正在查询 Hugging Face…";
    $("#hf-results").replaceChildren(loading);
    try {
      const models = await invoke("search_huggingface", { query: $("#model-search-query").value, category });
      if (requestId !== state.hfSearchSeq || category !== state.hfCategory) return;
      renderHfModels(models, category);
      $("#hf-result-summary").textContent = `${models.length} 项结果 · 按下载量从高到低，最多显示 20 项；结果由 Hugging Face API 返回。`;
    } catch (error) {
      if (requestId !== state.hfSearchSeq || category !== state.hfCategory) return;
      const failure = document.createElement("div"); failure.className = "error-box";
      const message = document.createElement("p"); message.textContent = `搜索失败：${error}`;
      const retry = document.createElement("button"); retry.className = "button secondary"; retry.textContent = "重试搜索";
      retry.addEventListener("click", () => $("#model-search-form").requestSubmit());
      failure.append(message, retry); $("#hf-results").replaceChildren(failure);
      $("#hf-result-summary").textContent = "请求失败；可检查网络、DNS 或系统代理设置后重试。";
    } finally {
      if (requestId === state.hfSearchSeq) { button.disabled = false; button.textContent = "搜索"; }
    }
  }

  function formatLocalDateTime(value) {
    if (!Number.isFinite(Number(value)) || Number(value) <= 0) return "时间未知";
    return new Date(Number(value)).toLocaleString();
  }
  function recurrenceLabel(value) {
    return value === "hourly" ? "每小时" : (value === "daily" ? "每 24 小时" : "仅一次");
  }
  function defaultScheduleInputTime() {
    const date = new Date(Date.now() + 5 * 60_000);
    date.setSeconds(0, 0);
    return new Date(date.getTime() - date.getTimezoneOffset() * 60_000).toISOString().slice(0, 16);
  }
  function renderScheduledTasks() {
    const box = $("#schedule-list");
    if (!box) return;
    box.replaceChildren();
    const tasks = state.scheduledTasks || [];
    $("#schedule-task-count").textContent = `${tasks.length} 项`;
    if (!tasks.length) {
      const empty = document.createElement("p");
      empty.className = "empty-state";
      empty.textContent = "还没有定时任务。创建后，应用会在打开期间检查到期时间。";
      box.append(empty);
      return;
    }
    tasks.slice().sort((a, b) => Number(a.nextRunAtMs || a.scheduledAtMs) - Number(b.nextRunAtMs || b.scheduledAtMs)).forEach((task) => {
      const card = document.createElement("article"); card.className = "schedule-task";
      const heading = document.createElement("div"); heading.className = "schedule-task-heading";
      const title = document.createElement("strong"); title.textContent = task.title;
      const finishedOnce = task.recurrence === "once" && task.lastRunAtMs != null;
      const status = document.createElement("span"); status.className = `schedule-status ${task.enabled ? "enabled" : "paused"}`;
      status.textContent = finishedOnce ? "已结束" : (task.enabled ? "已启用" : "已暂停");
      heading.append(title, status);
      const timing = document.createElement("p"); timing.className = "schedule-task-timing muted tiny";
      timing.textContent = `${recurrenceLabel(task.recurrence)} · ${task.nextRunAtMs ? `下次 ${formatLocalDateTime(task.nextRunAtMs)}` : "无待执行时间"}`;
      const prompt = document.createElement("p"); prompt.className = "schedule-task-prompt";
      prompt.textContent = task.prompt;
      card.append(heading, timing, prompt);
      if (task.lastStatus) {
        const statusLabel = task.lastStatus === "success" ? "成功" : (task.lastStatus === "skipped" ? "已跳过" : "失败");
        const last = document.createElement("div"); last.className = `schedule-last-result ${task.lastStatus === "success" ? "success" : (task.lastStatus === "skipped" ? "skipped" : "failed")}`;
        const lastTime = document.createElement("strong"); lastTime.textContent = `最近状态：${statusLabel} · ${formatLocalDateTime(task.lastRunAtMs)}`;
        const summary = document.createElement("p"); summary.textContent = task.lastSummary || "没有返回摘要";
        last.append(lastTime, summary); card.append(last);
      }
      if (Array.isArray(task.history) && task.history.length > 1) {
        const details = document.createElement("details"); details.className = "schedule-history";
        const summary = document.createElement("summary"); summary.textContent = `执行记录（${task.history.length}）`;
        const list = document.createElement("ul");
        task.history.slice().reverse().slice(0, 5).forEach((run) => {
          const item = document.createElement("li");
          const runStatus = run.status === "success" ? "成功" : (run.status === "skipped" ? "已跳过" : "失败");
          item.textContent = `${formatLocalDateTime(run.runAtMs)} · ${runStatus} · ${run.summary || "无摘要"}`;
          list.append(item);
        });
        details.append(summary, list); card.append(details);
      }
      const actions = document.createElement("div"); actions.className = "schedule-task-actions";
      const toggle = document.createElement("button"); toggle.className = "button secondary"; toggle.type = "button";
      toggle.textContent = finishedOnce ? "已结束" : (task.enabled ? "暂停" : "启用");
      toggle.disabled = finishedOnce;
      toggle.addEventListener("click", async () => {
        if (finishedOnce) return;
        toggle.disabled = true;
        try { await invoke("set_scheduled_task_enabled", { id: task.id, enabled: !task.enabled }); await loadScheduledTasks(); }
        catch (error) { showToast(`更新任务失败：${error}`, "error"); toggle.disabled = false; }
      });
      const remove = document.createElement("button"); remove.className = "button danger"; remove.type = "button"; remove.textContent = "删除";
      remove.addEventListener("click", async () => {
        if (!window.confirm(`删除定时任务“${task.title}”及其本机执行记录？`)) return;
        remove.disabled = true;
        try { await invoke("delete_scheduled_task", { id: task.id }); await loadScheduledTasks(); }
        catch (error) { showToast(`删除任务失败：${error}`, "error"); remove.disabled = false; }
      });
      actions.append(toggle, remove); card.append(actions); box.append(card);
    });
  }
  async function loadScheduledTasks() {
    const box = $("#schedule-list");
    if (!box) return;
    try {
      state.scheduledTasks = await invoke("list_scheduled_tasks");
      renderScheduledTasks();
    } catch (error) {
      box.replaceChildren();
      const failure = document.createElement("p"); failure.className = "error-box";
      failure.textContent = `读取定时任务失败：${error}`;
      const retry = document.createElement("button"); retry.className = "button secondary"; retry.type = "button"; retry.textContent = "重试";
      retry.addEventListener("click", loadScheduledTasks); box.append(failure, retry);
    }
  }
  async function createScheduledTask(event) {
    event.preventDefault();
    const button = $("#schedule-create");
    const scheduledAtMs = new Date($("#schedule-at").value).getTime();
    if (!Number.isFinite(scheduledAtMs)) { showToast("请选择有效的首次执行时间", "error"); return; }
    button.disabled = true; button.textContent = "正在保存…";
    try {
      await invoke("create_scheduled_task", {
        title: $("#schedule-title").value.trim(),
        prompt: $("#schedule-prompt").value.trim(),
        scheduledAtMs,
        recurrence: $("#schedule-recurrence").value,
      });
      $("#schedule-title").value = ""; $("#schedule-prompt").value = "";
      $("#schedule-recurrence").value = "once"; $("#schedule-at").value = defaultScheduleInputTime();
      await loadScheduledTasks(); showToast("定时任务已保存到本机");
    } catch (error) { showToast(`创建定时任务失败：${error}`, "error"); }
    finally { button.disabled = false; button.textContent = "创建定时任务"; }
  }

  function renderMonitorProcesses() {
    const box = $("#monitor-processes");
    if (!box) return;
    box.replaceChildren();
    const grouped = new Map();
    (state.modelProcesses || []).forEach((process) => {
      const current = grouped.get(process.name) || { name: process.name, count: 0, pids: [], cpu: 0, memory: 0 };
      current.count += 1; current.pids.push(process.pid);
      current.cpu += Number(process.cpuUsagePercent) || 0;
      current.memory += Number(process.memoryBytes) || 0;
      grouped.set(process.name, current);
    });
    if (!grouped.size) {
      const empty = document.createElement("p");
      empty.className = "muted tiny";
      empty.textContent = "当前未检测到名称匹配的 llama-server 或 stable-diffusion.cpp 进程。启动任务后采样可能需要数秒。";
      box.append(empty);
      return;
    }
    [...grouped.values()].forEach((process) => {
      const row = document.createElement("div"); row.className = "monitor-process-row";
      const name = document.createElement("strong"); name.textContent = `${process.name} · ${process.count} 个进程`;
      const detail = document.createElement("small");
      detail.textContent = `PID ${process.pids.join(", ")} · CPU 合计 ${process.cpu.toFixed(1)}% · 内存 ${fmtBytes(process.memory)}`;
      row.append(name, detail); box.append(row);
    });
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
      renderMonitorProcesses();
    } catch (error) {
      state.runtime = { ...state.runtime, running: false, lastError: `运行状态暂时无法读取：${error}` };
      $("#runtime-chip").textContent = "状态未知 · 无法确认本机服务";
      $("#runtime-chip").classList.remove("running");
      $("#run-status").textContent = "无法读取本机推理服务状态";
      $("#run-error").textContent = state.runtime.lastError;
      $("#run-error").hidden = false;
      renderRunCapabilityStatus();
      $("#stop-model").disabled = true;
      $("#chat-model-state").textContent = "状态未知 · 无法确认本地服务";
      $("#chat-model-select").disabled = true;
      $("#chat-input").disabled = true;
      $("#chat-local-port").textContent = "本地服务状态未知";
      $("#chat-local-port").dataset.lan = "false";
      $("#chat-local-port").title = "暂时无法读取本机服务状态；聊天已禁用以避免使用过期状态";
      renderMonitorProcesses();
      $("#chat-hint").textContent = "暂时无法读取本机服务状态，请先重试运行页";
      $("#monitor-api").textContent = "服务状态未知";
      updateChatSendState();
    }
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
      $("#settings-creations-dir").value = state.settings.creationsDirectory || "";
      $("#settings-appearance").value = state.settings.appearance || "system";
      $("#settings-port").value = state.settings.apiPort || 8686;
      applyAppearance(state.settings.appearance);
      renderModelStorage();
      renderComfyUiDirectoryState();
    } catch (error) { $("#settings-status").textContent = `无法读取设置：${error}`; }
  }
  function applyAppearance(value) {
    document.documentElement.dataset.theme = value === "dark" ? "dark" : (value === "light" ? "light" : "system");
  }
  async function saveSettings(event) {
    event.preventDefault();
    const settings = {
      ...(state.settings || {}),
      modelDirectory: $("#settings-model-dir").value.trim(),
      creationsDirectory: $("#settings-creations-dir").value.trim() || null,
      appearance: $("#settings-appearance").value,
      apiPort: Number($("#settings-port").value),
    };
    const status = $("#settings-status"); status.textContent = "正在保存…";
    try {
      state.settings = await invoke("save_app_settings", { settings });
      status.textContent = "已保存"; applyAppearance(settings.appearance);
      renderComfyUiDirectoryState();
      await Promise.all([refreshRuntime(), loadModels(), refreshModelStorage()]);
      showToast("设置已保存");
    } catch (error) { status.textContent = `保存失败：${error}`; }
  }
  async function chooseModelDirectory() {
    try {
      const path = await invoke("choose_directory");
      if (path) $("#settings-model-dir").value = path;
    } catch (error) { showToast(`选择目录失败：${error}`, "error"); }
  }
  async function chooseCreationsDirectory() {
    try {
      const path = await invoke("choose_directory");
      if (path) $("#settings-creations-dir").value = path;
    } catch (error) { showToast(`选择作品目录失败：${error}`, "error"); }
  }

  async function chooseOverviewModelDirectory() {
    const button = $("#overview-choose-model-dir");
    button.disabled = true;
    try {
      const path = await invoke("choose_directory");
      if (!path) return;
      const current = state.settings || await invoke("app_settings");
      state.settings = await invoke("save_app_settings", {
        settings: { ...current, modelDirectory: path },
      });
      $("#settings-model-dir").value = state.settings.modelDirectory;
      await Promise.all([refreshModelStorage(), loadModels()]);
      showToast("模型目录已更改；已有文件不会自动搬移");
    } catch (error) {
      showToast(`更改模型目录失败：${error}`, "error");
    } finally {
      button.disabled = false;
    }
  }

  async function refreshSystem() {
    try {
      const sample = await invoke("system_snapshot");
      state.modelProcesses = Array.isArray(sample.modelProcesses) ? sample.modelProcesses : [];
      renderMonitorProcesses();
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
    } catch {
      state.modelProcesses = [];
      renderMonitorProcesses();
    }
  }
  function drawChart() {
    const canvas = $("#system-chart");
    if (!canvas) return;
    const rect = canvas.getBoundingClientRect();
    if (!rect.width || !rect.height) return;
    const scale = window.devicePixelRatio || 1;
    canvas.width = Math.max(1, Math.round(rect.width * scale));
    canvas.height = Math.max(1, Math.round(rect.height * scale));
    const ctx = canvas.getContext("2d");
    ctx.setTransform(scale, 0, 0, scale, 0, 0);
    const width = rect.width; const height = rect.height;
    ctx.clearRect(0, 0, width, height);
    ctx.strokeStyle = getComputedStyle(document.documentElement).getPropertyValue("--line").trim() || "#e5ebe8";
    ctx.lineWidth = 1;
    ctx.fillStyle = getComputedStyle(document.documentElement).getPropertyValue("--muted").trim() || "#82908a";
    ctx.font = "11px system-ui";
    for (let i = 0; i <= 4; i++) {
      const y = height - (i / 4) * height * 0.85 - height * 0.06;
      ctx.beginPath(); ctx.moveTo(38, y); ctx.lineTo(width - 12, y); ctx.stroke();
      ctx.fillText(`${i * 25}%`, 2, y + 4);
    }
    const values = state.samples;
    const plotLeft = 42; const plotRight = width - 12; const plotTop = height * 0.06; const plotBottom = height * 0.91;
    const drawLine = (key, color) => {
      if (values.length < 2) return;
      ctx.strokeStyle = color; ctx.lineWidth = 2.5; ctx.beginPath();
      values.forEach((sample, index) => {
        const x = plotLeft + (index / Math.max(1, values.length - 1)) * (plotRight - plotLeft);
        const y = plotBottom - sample[key] / 100 * (plotBottom - plotTop);
        if (!index) ctx.moveTo(x, y); else ctx.lineTo(x, y);
      }); ctx.stroke();
    };
    drawLine("cpu", "#d87556"); drawLine("ram", "#4c82bb");
  }

  function switchModelTab(tab) {
    $$('[data-model-tab]').forEach((button) => {
      const active = button.dataset.modelTab === tab;
      button.classList.toggle("active", active);
      button.setAttribute("aria-selected", String(active));
      button.tabIndex = active ? 0 : -1;
    });
    $("#models-recommended").hidden = tab !== "recommended";
    $("#models-installed").hidden = tab !== "installed";
    $("#models-media").hidden = tab !== "media";
    $("#models-download").hidden = tab !== "download";
    if (tab === "recommended" && !state.recommendationsLoaded) searchRecommendationModels(state.recommendationCategory);
    if (tab === "media") renderMediaLibraryEngine();
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

  function parseMediaSize(value) {
    const match = /^(\d{3,4})x(\d{3,4})$/.exec(value || "");
    return match ? { width: Number(match[1]), height: Number(match[2]) } : { width: 0, height: 0 };
  }

  async function startMediaGeneration(kind) {
    if (state.mediaGenerationJobId) { showToast("已有画图或视频任务在运行", "error"); return; }
    const installed = state.engines.get("stable-diffusion.cpp")?.installedVersions || [];
    if (!installed.length) { showToast("请先安装官方 stable-diffusion.cpp CPU 引擎", "error"); return; }
    const video = kind === "video";
    const modelSelect = $(video ? "#video-model-select" : "#image-model-select");
    const size = parseMediaSize($(video ? "#video-size" : "#image-size").value);
    if (!modelSelect.value) { showToast("请先选择已安装或 ComfyUI 中的主模型", "error"); return; }
    if (video && (!$("#video-vae-select").value || !$("#video-text-select").value)) {
      showToast("Wan 视频必须选择兼容的 VAE 和 T5-XXL 编码器", "error"); return;
    }
    const prompt = $(video ? "#video-prompt" : "#image-prompt").value;
    if (!prompt.trim()) { showToast("请先输入提示词", "error"); return; }
    const jobId = globalThis.crypto?.randomUUID?.() || `media-job-${Date.now()}-${Math.random().toString(16).slice(2)}`;
    const generate = $(`#generate-${kind}`);
    const cancel = $(`#cancel-${kind}-generation`);
    const status = $(`#${kind}-generation-status`);
    const result = $(`#${kind}-result-path`);
    state.mediaGenerationJobId = jobId; state.mediaGenerationKind = kind;
    renderMonitorProcesses();
    generate.disabled = true; generate.textContent = "生成中…"; cancel.hidden = false; cancel.disabled = false;
    result.textContent = "";
    status.textContent = "正在启动 stable-diffusion.cpp 官方 sd-cli；本机为 CPU-only 构建，生成时间取决于模型和硬件。可随时取消。";
    const request = {
      kind,
      diffusionModelPath: modelSelect.value,
      vaePath: $(video ? "#video-vae-select" : "#image-vae-select").value || null,
      textEncoderPath: $(video ? "#video-text-select" : "#image-text-select").value || null,
      textEncoderMode: video ? "t5xxl" : $("#image-text-mode").value,
      cfgScale: 6.0,
      highNoiseModelPath: video ? ($("#video-high-noise-select").value || null) : null,
      prompt,
      negativePrompt: $(video ? "#video-negative-prompt" : "#image-negative-prompt").value,
      width: size.width,
      height: size.height,
      steps: Number($(video ? "#video-steps" : "#image-steps").value),
      videoFrames: video ? 33 : null,
    };
    try {
      const generated = await invoke("generate_media", { jobId, request });
      result.textContent = `已生成并保存：${generated.outputPath}`;
      status.textContent = "引擎已返回成功状态，且已确认非空作品文件存在。";
      showToast(video ? "视频生成完成" : "图片生成完成");
    } catch (error) {
      status.textContent = `生成失败或已取消：${error}`;
      showToast(`${video ? "视频" : "图片"}生成未完成`, "error");
    } finally {
      if (state.mediaGenerationJobId === jobId) {
        state.mediaGenerationJobId = null; state.mediaGenerationKind = null;
      }
      renderMonitorProcesses();
      generate.disabled = false; generate.textContent = video ? "生成视频" : "生成图片"; cancel.hidden = true;
    }
  }

  async function cancelMediaGeneration(kind) {
    const jobId = state.mediaGenerationJobId;
    if (!jobId || state.mediaGenerationKind !== kind) return;
    const button = $(`#cancel-${kind}-generation`); button.disabled = true; button.textContent = "正在停止…";
    try { await invoke("cancel_media_generation", { jobId }); }
    catch (error) { $(`#${kind}-generation-status`).textContent = `取消请求失败：${error}`; button.disabled = false; button.textContent = "取消生成"; }
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
        const progress = $$(".download-progress:not([hidden])").find((item) => item.dataset.downloadId === payload.downloadId);
        if (progress) progress.textContent = payload.total ? `下载中 ${Math.floor(payload.completed / payload.total * 100)}% · ${fmtBytes(payload.completed)} / ${fmtBytes(payload.total)}` : `已下载 ${fmtBytes(payload.completed)}`;
      });
      await listen("media-model-progress", (event) => {
        const payload = event.payload;
        const progress = $$(".download-progress:not([hidden])").find((item) => item.dataset.downloadId === payload.downloadId);
        if (progress) {
          const overall = payload.overallTotal ? Math.floor(payload.overallCompleted / payload.overallTotal * 100) : 0;
          progress.textContent = `组件 ${payload.fileName} · 当前 ${payload.total ? Math.floor(payload.completed / payload.total * 100) : 0}% · 总进度 ${overall}% · ${fmtBytes(payload.overallCompleted)} / ${fmtBytes(payload.overallTotal)}`;
        }
      });
      renderRunPreferences();
      $("#schedule-at").value = defaultScheduleInputTime();
      $("#chat-temperature").value = String(state.chatPreferences.temperature);
      $("#chat-temperature-value").textContent = state.chatPreferences.temperature.toFixed(1);
      $("#chat-include-history").checked = state.chatPreferences.includeHistory;
      await Promise.all([loadChatHistory(), loadHardware(), refreshModelStorage(), loadEngines(), loadModels(), loadSettings(), refreshRuntime(), refreshSystem()]);
      setInterval(refreshSystem, 2000);
      setInterval(refreshRuntime, 3000);
      setInterval(() => { if ($("#page-schedule").classList.contains("active")) loadScheduledTasks(); }, 5000);
      setPage(location.hash.slice(1) || "overview");
    } catch (error) { showToast(`桌面服务初始化失败：${error}`, "error"); }
  }

  window.addEventListener("hashchange", () => setPage(location.hash.slice(1)));
  window.addEventListener("resize", () => { if ($("#page-monitor").classList.contains("active")) drawChart(); });
  $("#refresh-hardware").addEventListener("click", loadHardware);
  $("#quickstep-refresh-hardware").addEventListener("click", loadHardware);
  $("#refresh-model-storage").addEventListener("click", refreshModelStorage);
  $("#overview-choose-model-dir").addEventListener("click", chooseOverviewModelDirectory);
  $("#overview-recommended-models").addEventListener("click", () => switchModelTab("recommended"));
  $("#overview-all-models").addEventListener("click", (event) => { event.preventDefault(); openHuggingFaceLibrary("all"); });
  $("#refresh-engines").addEventListener("click", loadEngines);
  $("#pick-model").addEventListener("click", importLocalModel);
  $("#model-search-form").addEventListener("submit", searchHf);
  $$ ("[data-recommend-category]").forEach((button) => button.addEventListener("click", () => searchRecommendationModels(button.dataset.recommendCategory)));
  $$("[data-hf-category]").forEach((button) => button.addEventListener("click", () => {
    if (setHfCategory(button.dataset.hfCategory)) $("#model-search-form").requestSubmit();
  }));
  $$('[data-model-tab]').forEach((button) => {
    button.addEventListener("click", () => switchModelTab(button.dataset.modelTab));
    button.addEventListener("keydown", (event) => {
      const tabs = $$('[data-model-tab]');
      const index = tabs.indexOf(button);
      let nextIndex = index;
      if (event.key === "ArrowRight") nextIndex = (index + 1) % tabs.length;
      else if (event.key === "ArrowLeft") nextIndex = (index - 1 + tabs.length) % tabs.length;
      else if (event.key === "Home") nextIndex = 0;
      else if (event.key === "End") nextIndex = tabs.length - 1;
      else return;
      event.preventDefault();
      switchModelTab(tabs[nextIndex].dataset.modelTab);
      tabs[nextIndex].focus();
    });
  });
  $$("[data-image-view]").forEach((button) => button.addEventListener("click", () => switchImageView(button.dataset.imageView)));
  $("#open-creations-directory").addEventListener("click", (event) => openCreationsDirectory("image", event.currentTarget));
  $("#open-video-directory").addEventListener("click", (event) => openCreationsDirectory("video", event.currentTarget));
  $("#open-image-model-library").addEventListener("click", () => openHuggingFaceLibrary("image", "diffusion"));
  $("#open-video-model-library").addEventListener("click", () => openHuggingFaceLibrary("video", "Wan"));
  $("#prepare-qwen-image").addEventListener("click", () => prepareRecommendedMedia("image", "Comfy-Org/Qwen-Image-2.1", [
    "diffusion_models/qwen_image_2.1_int8_convrot.safetensors",
    "vae/qwen_image_2.1_vae_bf16.safetensors",
    "text_encoders/qwen3vl_8b_int8_convrot.safetensors",
  ]));
  $("#prepare-sd15-image").addEventListener("click", () => prepareRecommendedMedia("image", "stable-diffusion-v1-5/stable-diffusion-v1-5", [
    "v1-5-pruned-emaonly.safetensors",
  ]));
  $("#prepare-wan-video").addEventListener("click", () => prepareRecommendedMedia("video", "Comfy-Org/Wan_2.1_ComfyUI_repackaged", [
    "split_files/diffusion_models/wan2.1_t2v_1.3B_fp16.safetensors",
    "split_files/vae/wan_2.1_vae.safetensors",
    "split_files/text_encoders/umt5_xxl_fp16.safetensors",
  ]));
  $("#image-model-manager-link").addEventListener("click", (event) => { event.preventDefault(); switchModelTab("media"); location.hash = "#models"; setPage("models"); });
  $("#generate-image").addEventListener("click", () => startMediaGeneration("image"));
  $("#generate-video").addEventListener("click", () => startMediaGeneration("video"));
  $("#cancel-image-generation").addEventListener("click", () => cancelMediaGeneration("image"));
  $("#cancel-video-generation").addEventListener("click", () => cancelMediaGeneration("video"));
  ["#image-model-select", "#image-vae-select", "#image-text-select"].forEach((selector) => $(selector).addEventListener("change", () => updateMediaResourceStatus("image")));
  $("#image-text-mode").addEventListener("change", () => updateMediaResourceStatus("image"));
  ["#video-model-select", "#video-high-noise-select", "#video-vae-select", "#video-text-select"].forEach((selector) => $(selector).addEventListener("change", () => updateMediaResourceStatus("video")));
  $("#refresh-media-models").addEventListener("click", refreshMediaModels);
  $("#choose-comfyui-directory").addEventListener("click", chooseComfyUiDirectory);
  $("#scan-comfyui-directory").addEventListener("click", () => scanComfyUiDirectory(state.settings?.comfyuiDirectory));
  $("#media-engine-check").addEventListener("click", checkMediaEngineVersion);
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
    $("#toggle-chat-sidebar").setAttribute("aria-expanded", String(!collapsed));
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
  $("#choose-creations-dir").addEventListener("click", chooseCreationsDirectory);
  $("#settings-appearance").addEventListener("change", (event) => applyAppearance(event.target.value));
  $("#monitor-refresh").addEventListener("click", refreshSystem);
  $("#schedule-refresh").addEventListener("click", loadScheduledTasks);
  $("#schedule-form").addEventListener("submit", createScheduledTask);
  $$("[data-jump]").forEach((button) => button.addEventListener("click", () => { location.hash = `#${button.dataset.jump}`; }));
  start();
})();
