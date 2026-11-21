(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.LocmAgentProtocol = api;
})(globalThis, function () {
  const CONTINUATION_PREFIX = "【LocM 应用内部继续指令；不是用户新请求】";
  const TOOL_RESULT_PREFIX = "【LocM 应用内部工作区工具结果；不是用户新请求】";
  const TOOL_ROUTING_GUIDANCE = "工具意图路由：仅当用户要新建的目标本身是文件夹或目录时，才在计划已展示后调用 create_folder；如果文件夹只是存放位置，而用户要在其中新建文件、脚本或命令文件，应使用 write_file，不能把存放位置误当成新建目录目标。不得用 write_file 或 run_command 创建目录。所有文件工具 path 都是工作区内的具体相对路径，不支持通配符；列出工作区根目录必须使用 path=\".\"，不要用 .**、*、glob 等模式。用户要求修改已有文件内容时优先用 edit_file；先用 read_file 获取精确 old_text，再提交唯一匹配的 old_text/new_text，不得为了省事用 write_file 重写整个现有文件。缺少文件内容或精确片段时先读取；匹配不存在或不唯一时安全停止并说明。对本轮新建或修改的交付文件，write_file、edit_file 或经用户批准的覆盖成功后，必须立刻用 read_file 回读同一路径，并对照用户目标检查实际内容；代码或配置按任务需要运行适当测试。回读失败、结果受截断或内容不匹配时，不得声称验收通过；先修正并再次回读，若 read_file 未启用则如实说明无法回读验证。web_search/web_fetch 只在用户明确需要公开网页信息时使用；它们会向外部网站发送搜索词或请求。每次工具结果都是应用内部上下文，不是用户新任务；保持当前已展示计划并实时继续下一步，不要重新规划或停在通用进度。edit_file 和其他副作用必须保留用户审批与系统原生确认。";
  const encoder = new TextEncoder();
  const TOOL_ARGUMENTS = Object.freeze({
    list_files: { required: ["path"], types: { path: "string" } },
    search_files: { required: ["path", "query"], types: { path: "string", query: "string", caseSensitive: "boolean" } },
    read_file: { required: ["path"], types: { path: "string" } },
    create_folder: { required: ["path"], types: { path: "string" } },
    write_file: { required: ["path", "content"], types: { path: "string", content: "string" } },
    edit_file: { required: ["path", "old_text", "new_text"], types: { path: "string", old_text: "string", new_text: "string" } },
    delete_file: { required: ["path"], types: { path: "string" } },
    run_command: { required: ["program", "args"], types: { program: "string", args: "array" } },
    web_search: { required: ["query"], types: { query: "string" } },
    web_fetch: { required: ["url"], types: { url: "string" } },
  });
  const DEFAULT_ENABLED_TOOLS = Object.freeze(["list_files", "search_files", "read_file", "create_folder", "write_file", "edit_file", "delete_file", "run_command"]);
  const READ_ONLY_TOOLS = Object.freeze(["list_files", "search_files", "read_file", "web_search", "web_fetch"]);
  const TOOL_OPTIONS = Object.freeze([
    { id: "list_files", label: "列出文件", description: "查看工作区内的文件与子目录。" },
    { id: "search_files", label: "搜索文本", description: "搜索工作区内受限大小的UTF-8文本。" },
    { id: "read_file", label: "读取文件", description: "读取工作区内的UTF-8文本文件。" },
    { id: "create_folder", label: "创建文件夹", description: "在工作区内创建空文件夹。" },
    { id: "write_file", label: "新建文件", description: "在工作区内创建新文件；覆盖仍需确认。" },
    { id: "edit_file", label: "编辑文件", description: "按唯一精确匹配修改文件；需要批准。" },
    { id: "delete_file", label: "删除文件", description: "删除工作区内的普通文件；需要批准。" },
    { id: "run_command", label: "运行命令", description: "在工作区目录启动程序；需要批准。" },
    { id: "web_search", label: "网页搜索", description: "向外部搜索服务发送查询词；默认关闭，仅在需要公开网页信息时启用。" },
    { id: "web_fetch", label: "读取网页", description: "读取受限的公开 HTTP(S) 网页；不会访问本机或私有地址。" },
  ]);
  function normalizeEnabledTools(value) {
    const all = TOOL_OPTIONS.map((tool) => tool.id);
    if (!Array.isArray(value)) return [...DEFAULT_ENABLED_TOOLS];
    const supplied = new Set(value.filter((item) => typeof item === "string"));
    if (value.length && !all.some((tool) => supplied.has(tool))) return [...DEFAULT_ENABLED_TOOLS];
    return all.filter((tool) => supplied.has(tool));
  }

  function normalizeScope(value) { return value === "read_only" ? "read_only" : "workspace"; }
  function enabledToolsForScope(enabledTools, scope) {
    const enabled = normalizeEnabledTools(enabledTools);
    return normalizeScope(scope) === "read_only" ? enabled.filter((name) => READ_ONLY_TOOLS.includes(name)) : enabled;
  }

  function setAllToolsEnabled(enabled) {
    return normalizeEnabledTools(enabled ? TOOL_OPTIONS.map((tool) => tool.id) : []);
  }

  function masterToolSwitchState(enabledTools) {
    const enabledCount = normalizeEnabledTools(enabledTools).length;
    return {
      checked: enabledCount === TOOL_OPTIONS.length,
      indeterminate: enabledCount > 0 && enabledCount < TOOL_OPTIONS.length,
    };
  }

  function appendContinuation(task, instruction) {
    if (!task || !Array.isArray(task.turns)) throw new TypeError("task.turns must be an array");
    task.turns.push({
      role: "user",
      internalType: "continuation",
      content: `${CONTINUATION_PREFIX}\n${String(instruction || "请继续处理当前任务。")}`,
    });
  }

  function appendToolResult(task, result) {
    if (!task || !Array.isArray(task.turns)) throw new TypeError("task.turns must be an array");
    task.turns.push({ role: "user", internalType: "tool_result", content: `${TOOL_RESULT_PREFIX}\n${String(result ?? "")}` });
  }

  function isClarificationRequest(text) {
    const source = String(text ?? "").trim();
    if (!source) return false;
    const chineseRequest = /(?:请(?:提供|补充|说明|明确|描述|告诉)[\s\S]{0,80}(?:任务|目标|需求|详情|信息|工作区|输入|输出|文件|路径)|(?:任务|目标|需求|详情|信息|工作区|输入|输出|文件|路径)[\s\S]{0,32}(?:请提供|请补充|请说明|请明确|还需要)|(?:你(?:能否|可以|能不能)|是否可以)[\s\S]{0,20}(?:提供|补充|说明|明确|描述|告诉))/iu;
    const englishRequest = /\b(?:please (?:provide|clarify|specify|describe|tell me)|could you (?:provide|clarify|specify|describe|tell me)|i need to understand|i need (?:you to )?(?:provide|clarify|specify|tell me)|the specific task you want|relevant information about (?:the )?current work area)\b/i;
    const topic = /\b(?:task|goal|details|information|work area|workspace|input|output|file|path)\b/i;
    return chineseRequest.test(source) || (englishRequest.test(source) && topic.test(source));
  }

  function parseObjectCandidate(source) {
    try {
      const value = JSON.parse(source);
      return value && typeof value === "object" && !Array.isArray(value) ? value : null;
    } catch {
      return null;
    }
  }

  function repairIncompletePlan(source) {
    const match = String(source ?? "").trim().match(/^(\{\s*"type"\s*:\s*"plan"\s*,\s*"steps"\s*:\s*\[[\s\S]*)\}\s*\)?$/);
    if (!match) return null;
    let value;
    try {
      value = JSON.parse(`${match[1]}]}`);
    } catch {
      return null;
    }
    if (value?.type !== "plan" || !Array.isArray(value.steps) || !value.steps.every((step) => typeof step === "string")) return null;
    if (Object.keys(value).length !== 2) return null;
    return value;
  }

  function balancedContainerEnd(source, start) {
    const expectedClosers = [];
    let inString = false;
    let escaped = false;
    for (let index = start; index < source.length; index++) {
      const character = source[index];
      if (inString) {
        if (escaped) escaped = false;
        else if (character === "\\") escaped = true;
        else if (character === '"') inString = false;
        continue;
      }
      if (character === '"') {
        inString = true;
      } else if (character === "{") {
        expectedClosers.push("}");
      } else if (character === "[") {
        expectedClosers.push("]");
      } else if (character === expectedClosers.at(-1)) {
        expectedClosers.pop();
        if (!expectedClosers.length) return index;
      } else if (character === "}" || character === "]") {
        return -1;
      }
    }
    return -1;
  }

  function decodeXmlText(text) {
    return String(text).replace(/&(#x[\da-f]+|#\d+|amp|lt|gt|quot|apos);/giu, (entity, code) => {
      const named = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'" };
      if (Object.hasOwn(named, code)) return named[code];
      const numeric = code[0].toLowerCase() === "x"
        ? Number.parseInt(code.slice(1), 16)
        : code[0] === "#" ? Number.parseInt(code.slice(1), 10) : NaN;
      try { return Number.isInteger(numeric) ? String.fromCodePoint(numeric) : entity; }
      catch { return entity; }
    });
  }

  function parseMiniCpm5ToolCall(text) {
    const source = String(text ?? "");
    const openings = source.match(/<function\b/giu) || [];
    const closings = source.match(/<\/function\s*>/giu) || [];
    const blocks = [...source.matchAll(/<function\b[\s\S]*?<\/function\s*>/giu)];
    if (openings.length !== 1 || closings.length !== 1 || blocks.length !== 1) return null;
    const block = blocks[0][0];
    const open = block.match(/^<function\s+name\s*=\s*(?:"([^"<>]+)"|'([^'<>]+)')[^>]*>/u);
    if (!open) return null;
    const name = (open[1] || open[2] || "").trim();
    const schema = TOOL_ARGUMENTS[name];
    if (!schema) return null;
    const body = block.slice(open[0].length).replace(/<\/function\s*>$/u, "");
    const parameters = {};
    const parameterPattern = /<param\s+name\s*=\s*(?:"([^"<>]+)"|'([^'<>]+)')[^>]*>([\s\S]*?)<\/param\s*>/giu;
    let cursor = 0;
    for (const match of body.matchAll(parameterPattern)) {
      if (body.slice(cursor, match.index).trim()) return null;
      const key = (match[1] || match[2] || "").trim();
      if (!key || !Object.hasOwn(schema.types, key) || Object.hasOwn(parameters, key)) return null;
      let raw = match[3].trim();
      if (raw.startsWith("<![CDATA[") && raw.endsWith("]]>") ) {
        raw = raw.slice(9, -3);
      } else {
        if (/<[A-Za-z!/]/u.test(raw)) return null;
        raw = decodeXmlText(raw);
      }
      if (/&(?:[A-Za-z#]|$)/u.test(raw)) return null;
      const expectedType = schema.types[key];
      let value = raw;
      if (expectedType !== "string") {
        try { value = JSON.parse(raw); }
        catch { return null; }
      }
      if (expectedType === "array" ? !Array.isArray(value) : typeof value !== expectedType) return null;
      parameters[key] = value;
      cursor = match.index + match[0].length;
    }
    if (body.slice(cursor).trim() || schema.required.some((key) => !Object.hasOwn(parameters, key))) return null;
    try {
      return parseToolAction({ type: "tool_call", name, ...parameters });
    } catch {
      return null;
    }
  }

  function parseAgentJson(text) {
    const candidate = String(text ?? "").trim().replace(/^\uFEFF/, "");
    if (!candidate) return null;
    const miniCpmToolCall = parseMiniCpm5ToolCall(candidate);
    if (miniCpmToolCall) return { type: "tool_call", ...miniCpmToolCall };
    try {
      const value = JSON.parse(candidate);
      return value && typeof value === "object" && !Array.isArray(value) ? value : null;
    } catch {
      // Local models sometimes wrap one JSON object in a single Markdown fence or short prose.
    }

    const fences = [...candidate.matchAll(/```(?:json)?[ \t]*\r?\n?([\s\S]*?)```/gi)];
    if (fences.length) {
      if (fences.length !== 1) return null;
      const fenced = fences[0][1].trim();
      return parseObjectCandidate(fenced) || repairIncompletePlan(fenced);
    }
    const repairedPlan = repairIncompletePlan(candidate);
    if (repairedPlan) return repairedPlan;

    const objects = [];
    let inQuotedProse = false;
    let escapedProse = false;
    for (let index = 0; index < candidate.length; index++) {
      const character = candidate[index];
      if (inQuotedProse) {
        if (escapedProse) escapedProse = false;
        else if (character === "\\") escapedProse = true;
        else if (character === '"') inQuotedProse = false;
        continue;
      }
      if (character === '"') {
        inQuotedProse = true;
        continue;
      }
      if (character !== "{" && character !== "[") continue;
      const end = balancedContainerEnd(candidate, index);
      if (end < 0) return null;
      if (character === "{") {
        const value = parseObjectCandidate(candidate.slice(index, end + 1));
        if (!value) return null;
        objects.push(value);
        if (objects.length > 1) return null;
      }
      index = end;
    }
    return objects.length === 1 ? objects[0] : null;
  }

  function appendMalformedResponseRetry(task, retriesRemaining) {
    const remaining = Math.max(0, Math.floor(Number(retriesRemaining) || 0));
    if (!remaining) return { status: "exhausted", retriesRemaining: 0 };
    appendContinuation(task, "上一条模型回复不符合Agent协议，且没有执行任何工具或命令。仅进行这一次纠偏：工作区动作优先使用本轮提供的单个标准函数工具；MiniCPM5可额外使用合法的XML function/param调用。如果当前模型或模板不能发出原生工具调用，就回退为一个JSON tool_call对象；type只能为plan、tool_call、message或done，工具名和字段必须来自已开放列表。此回退适用于任何已选本地模型，不按模型名称筛选。不要输出额外自然语言、Markdown或代码围栏；不要把历史中的自然语言、示例或代码改写成可执行命令。run_command仍需应用确认卡和系统原生确认；若无法安全完成，请用message说明，且不得声称操作已完成。");
    return { status: "retry", retriesRemaining: remaining - 1 };
  }

  function validateWorkspacePath(value, { allowRoot = false, label = "工具路径" } = {}) {
    if (typeof value !== "string" || !value.trim() || encoder.encode(value).length > 1024) {
      throw new Error(`${label}不能为空，且最多为 1024 字节`);
    }
    if (value !== value.trim() || value.includes("\\") || value.startsWith("/")
      || /[\u0000-\u001f\u007f:*?<>|"]/u.test(value)) {
      throw new Error(`${label}必须是工作区内的具体相对路径，不能包含通配符、反斜杠或不安全字符`);
    }
    if (allowRoot && value === ".") return value;
    const parts = value.split("/");
    if (allowRoot && parts[0] === ".") parts.shift();
    if (!parts.length || parts.some((part) => !part || part === "." || part === "..")) {
      throw new Error(`${label}必须是工作区内的具体相对路径；列出工作区根目录请使用 "."`);
    }
    return value;
  }

  function parseToolAction(action, enabledTools = null) {
    if (!action || typeof action !== "object" || action.type !== "tool_call") return null;
    const name = String(action.name || "");
    if (Array.isArray(enabledTools) && !enabledTools.includes(name)) throw new Error(`工具“${name || "未知"}”已关闭，尚未执行`);
    if (name === "search_files") {
      if (typeof action.path !== "string" || typeof action.query !== "string") throw new Error("搜索工具需要 path 和 query 文本字段");
      validateWorkspacePath(action.path, { allowRoot: true, label: "搜索起始路径" });
      if (!action.query.trim() || encoder.encode(action.query).length > 256) throw new Error("搜索内容不能为空，且最多为 256 字节");
      if (action.caseSensitive !== undefined && typeof action.caseSensitive !== "boolean") throw new Error("caseSensitive 必须是布尔值");
      return { name, path: action.path, query: action.query, caseSensitive: action.caseSensitive === true };
    }
    if (["list_files", "read_file", "create_folder"].includes(name)) {
      // A few local models emit this exact glob when they mean “inspect the
      // workspace root”. Translate it only for this read-only listing tool;
      // all other wildcard paths remain invalid and never reach the backend.
      const correctedFrom = name === "list_files" && action.path === ".**" ? ".**" : null;
      const path = validateWorkspacePath(correctedFrom ? "." : action.path, {
        allowRoot: name === "list_files",
        label: name === "create_folder" ? "文件夹路径" : name === "read_file" ? "文件路径" : "列目录路径",
      });
      return correctedFrom ? { name, path, correctedFrom } : { name, path };
    }
    if (name === "write_file") {
      if (typeof action.path !== "string" || typeof action.content !== "string") throw new Error("写入工具需要 path 和 content 文本字段");
      const path = validateWorkspacePath(action.path, { label: "文件路径" });
      if (encoder.encode(action.content).length > 1024 * 1024) throw new Error("单个文件不能超过 1 MiB");
      return { name, path, content: action.content };
    }
    if (name === "edit_file") {
      if (typeof action.path !== "string" || typeof action.old_text !== "string" || typeof action.new_text !== "string") {
        throw new Error("编辑工具需要 path、old_text 和 new_text 文本字段");
      }
      const path = validateWorkspacePath(action.path, { label: "编辑文件路径" });
      if (!action.old_text || encoder.encode(action.old_text).length > 1024 * 1024) throw new Error("old_text 不能为空且最多为 1 MiB");
      if (encoder.encode(action.new_text).length > 1024 * 1024) throw new Error("new_text 最多为 1 MiB");
      return { name, path, old_text: action.old_text, new_text: action.new_text };
    }
    if (name === "delete_file") {
      if (typeof action.path !== "string") throw new Error("删除工具路径无效");
      return { name, path: validateWorkspacePath(action.path, { label: "删除文件路径" }) };
    }
    if (name === "run_command") {
      if (typeof action.program !== "string" || !Array.isArray(action.args) || action.args.some((arg) => typeof arg !== "string")) {
        throw new Error("命令工具需要 program 文本和 args 字符串数组");
      }
      return { name, program: action.program, args: action.args };
    }
    if (name === "web_search") {
      if (typeof action.query !== "string" || !action.query.trim() || encoder.encode(action.query).length > 256 || /[\u0000-\u001f\u007f]/u.test(action.query)) {
        throw new Error("网页搜索词不能为空、不能含控制字符，且最多为 256 字节");
      }
      return { name, query: action.query.trim() };
    }
    if (name === "web_fetch") {
      if (typeof action.url !== "string" || !action.url.trim() || encoder.encode(action.url).length > 2048 || /[\u0000-\u001f\u007f]/u.test(action.url)) {
        throw new Error("网页地址不能为空、不能含控制字符，且最多为 2048 字节");
      }
      let parsed;
      try { parsed = new URL(action.url); } catch { throw new Error("网页地址无效"); }
      if (!["http:", "https:"].includes(parsed.protocol) || parsed.username || parsed.password) throw new Error("只允许不含凭据的 HTTP(S) 网页地址");
      return { name, url: parsed.href };
    }
    throw new Error(`模型请求了未开放的工具：${name || "（空名称）"}`);
  }

  function createFolderDispatch(workspace, relativePath, taskId, permissionToken) {
    if (typeof workspace !== "string" || !workspace) throw new TypeError("workspace must be a non-empty string");
    try { validateWorkspacePath(relativePath, { label: "文件夹路径" }); }
    catch (error) { throw new TypeError(String(error.message || error)); }
    if (typeof taskId !== "string" || !taskId || encoder.encode(taskId).length > 128) throw new TypeError("taskId must be a non-empty string of at most 128 bytes");
    if (typeof permissionToken !== "string" || !/^[0-9a-f]{64}$/.test(permissionToken)) throw new TypeError("permissionToken must be a 256-bit hex capability");
    return { command: "agent_create_folder", args: { workspace, relativePath, taskId, permissionToken } };
  }

  function editFileDispatch(workspace, action, approved = false, taskId, permissionToken) {
    if (typeof workspace !== "string" || !workspace) throw new TypeError("workspace must be a non-empty string");
    if (typeof approved !== "boolean") throw new TypeError("approved must be a boolean");
    if (typeof taskId !== "string" || !taskId || encoder.encode(taskId).length > 128) throw new TypeError("taskId must be a non-empty string of at most 128 bytes");
    if (typeof permissionToken !== "string" || !/^[0-9a-f]{64}$/.test(permissionToken)) throw new TypeError("permissionToken must be a 256-bit hex capability");
    const parsed = parseToolAction({
      type: "tool_call",
      name: "edit_file",
      path: action?.path,
      old_text: action?.old_text,
      new_text: action?.new_text,
    });
    return {
      command: "agent_edit_file",
      args: {
        workspace,
        relativePath: parsed.path,
        oldText: parsed.old_text,
        newText: parsed.new_text,
        allowEdit: approved,
        taskId,
        permissionToken,
      },
    };
  }

  function preferredToolsForUserRequest(request) {
    const text = String(request ?? "");
    const folderRequested = /(?:创建|新建|建立|增加|新增|添加|加|建(?:一个|个)?)[\s\S]{0,48}(?:文件夹|目录)|\b(?:create|make|add|new)\b[\s\S]{0,64}\b(?:folder|directory)\b/i.test(text);
    const folderDenied = /(?:不要|别|无需|不需要|禁止)[\s\S]{0,16}(?:创建|新建|建立|增加|新增|添加|加)[\s\S]{0,48}(?:文件夹|目录)|\b(?:do not|don't|avoid|without)\b[\s\S]{0,64}\b(?:create|make|add|new)\b[\s\S]{0,64}\b(?:folder|directory)\b/i.test(text);
    const editVerb = /(?:修改|编辑|更改|改动|改写|改|更新|修复|替换|补充)|\b(?:edit|modify|change|update|fix|rewrite|replace)\b/i.test(text);
    const fileTarget = /(?:现有|已有)?(?:工作区)?(?:文件|文档|脚本|代码)|\b(?:file|document|script|code)\b|\b[\w.-]+\.(?:md|txt|rs|js|json|toml|yaml|yml|css|html|py|ts|tsx|jsx|sh|bat|c|cpp|h|hpp|go|java|kt|swift)\b/i.test(text);
    const editDenied = /(?:不要|别|无需|不需要|不用|禁止)[\s\S]{0,16}(?:修改|编辑|更改|改动|改写|改|更新|修复|替换)|\b(?:do not|don't|avoid|without)\b[\s\S]{0,32}\b(?:edit|modify|change|update|fix|rewrite|replace)\b/i.test(text);
    const tools = [];
    if (folderRequested && !folderDenied) tools.push("create_folder");
    if (editVerb && fileTarget && !editDenied) tools.push("edit_file");
    if (/(?:搜索|搜一下|查一下|网上查|网页搜索|web search|search the web|look up online)/iu.test(text)) tools.push("web_search");
    if (/(?:读取网页|抓取网页|打开这个网页|访问这个网址|web fetch|fetch this page)/iu.test(text)) tools.push("web_fetch");
    return tools;
  }

  function appendDuplicatePlanRetry(task, retriesRemaining) {
    const remaining = Math.max(0, Math.floor(Number(retriesRemaining) || 0));
    if (!remaining) return { status: "exhausted", retriesRemaining: 0 };
    appendContinuation(task, "计划已经展示，上一条模型回复重复生成了计划。这不是用户新请求；不要重新生成或修改计划。请根据已展示计划直接执行下一步并返回对应的唯一 JSON 动作；若下一步需要检查工作区，使用 list_files；若任务已完成，直接总结。不得声称尚未执行的操作已经完成。");
    return { status: "retry", retriesRemaining: remaining - 1 };
  }

  function validatePlan(parsed) {
    const steps = Array.isArray(parsed?.steps)
      ? parsed.steps.filter((step) => typeof step === "string").map((step) => step.trim()).filter(Boolean).slice(0, 12)
      : [];
    const hasPlaceholderStep = steps.some((step) => /^(?:步骤\s*[一二三四五六七八九十百\d]+|step\s*\d+)$/iu.test(step));
    const valid = steps.length >= 1
      && steps.length === parsed.steps.length
      && steps.every((step) => encoder.encode(step).length <= 500)
      && !hasPlaceholderStep;
    return { valid, steps };
  }

  function recordPlanResponse(task, raw, parsed, options = {}) {
    if (parsed?.type !== "plan") return { status: "not_plan", steps: [] };
    if (!task.needsPlan && options.allowReplan !== true) {
      // Preserve the model response for audit/history. The caller adds a
      // user-role result and pauses without rendering a second plan card.
      task.turns.push({ role: "assistant", content: String(raw ?? "") });
      return { status: "duplicate", steps: [] };
    }
    const { valid, steps } = validatePlan(parsed);
    if (!valid) return { status: "invalid", steps };

    task.plan = steps;
    task.needsPlan = false;
    task.turns.push({ role: "assistant", content: JSON.stringify({ type: "plan", steps }) });
    if (options.continueAfterPlan === true) {
      appendContinuation(task, "计划已经展示。请按该计划从第一步开始执行，不要重复计划；需要文件操作时调用对应工具，完成后再总结。");
    }
    return { status: "accepted", steps };
  }

  function removeDuplicatePlanEntries(entries) {
    const result = [];
    for (const entry of Array.isArray(entries) ? entries : []) {
      const previous = result.at(-1);
      const isPlanEntry = entry?.kind === "assistant"
        && typeof entry.text === "string"
        && entry.text.startsWith("执行计划（");
      if (isPlanEntry && previous?.kind === "assistant" && previous.text === entry.text) continue;
      result.push(entry);
    }
    return result;
  }

  return { CONTINUATION_PREFIX, TOOL_RESULT_PREFIX, TOOL_ROUTING_GUIDANCE, TOOL_OPTIONS, DEFAULT_ENABLED_TOOLS, READ_ONLY_TOOLS, normalizeEnabledTools, normalizeScope, enabledToolsForScope, setAllToolsEnabled, masterToolSwitchState, appendContinuation, appendToolResult, isClarificationRequest, parseAgentJson, parseMiniCpm5ToolCall, appendMalformedResponseRetry, parseToolAction, createFolderDispatch, editFileDispatch, preferredToolsForUserRequest, appendDuplicatePlanRetry, validatePlan, recordPlanResponse, removeDuplicatePlanEntries };
});
