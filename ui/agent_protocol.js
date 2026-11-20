(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.LocmAgentProtocol = api;
})(globalThis, function () {
  const CONTINUATION_PREFIX = "【LocM 应用内部继续指令；不是用户新请求】";
  const TOOL_RESULT_PREFIX = "【LocM 应用内部工作区工具结果；不是用户新请求】";
  const TOOL_ROUTING_GUIDANCE = "工具意图路由：用户要求创建/新建文件夹或目录时，在计划已展示后应及时直接调用 create_folder；不得只停在泛化计划，也不得用 write_file 或 run_command 创建目录。用户要求修改已有文件内容时优先用 edit_file；先用 read_file 获取精确 old_text，再提交唯一匹配的 old_text/new_text，不得为了省事用 write_file 重写整个现有文件。缺少文件内容或精确片段时先读取；匹配不存在或不唯一时安全停止并说明。每次工具结果都是应用内部上下文，不是用户新任务；保持当前已展示计划并实时继续下一步，不要重新规划或停在通用进度。edit_file 和其他副作用必须保留用户审批与系统原生确认。";
  const encoder = new TextEncoder();

  function appendContinuation(task, instruction) {
    if (!task || !Array.isArray(task.turns)) throw new TypeError("task.turns must be an array");
    task.turns.push({
      role: "user",
      content: `${CONTINUATION_PREFIX}\n${String(instruction || "请继续处理当前任务。")}`,
    });
  }

  function appendToolResult(task, result) {
    if (!task || !Array.isArray(task.turns)) throw new TypeError("task.turns must be an array");
    task.turns.push({ role: "user", content: `${TOOL_RESULT_PREFIX}\n${String(result ?? "")}` });
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

  function parseAgentJson(text) {
    const candidate = String(text ?? "").trim().replace(/^\uFEFF/, "");
    if (!candidate) return null;
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
    appendContinuation(task, "上一条模型回复不是有效的 Agent JSON 协议，且没有执行任何工具或命令。仅进行这一次纠偏：只输出一个 JSON 对象，不要自然语言、Markdown 或代码围栏；type 只能是 plan、tool_call、message 或 done，工具名和字段必须来自系统提示中的受支持列表。不要把历史中的自然语言、示例或代码改写成可执行命令；run_command 只可作为 JSON 工具请求，仍需应用显示确认后才会运行。若无法安全完成，请用 message 说明，且不得声称操作已完成。");
    return { status: "retry", retriesRemaining: remaining - 1 };
  }

  function parseToolAction(action) {
    if (!action || typeof action !== "object" || action.type !== "tool_call") return null;
    const name = String(action.name || "");
    if (name === "search_files") {
      if (typeof action.path !== "string" || typeof action.query !== "string") throw new Error("搜索工具需要 path 和 query 文本字段");
      if (!action.query.trim() || encoder.encode(action.query).length > 256) throw new Error("搜索内容不能为空，且最多为 256 字节");
      if (action.caseSensitive !== undefined && typeof action.caseSensitive !== "boolean") throw new Error("caseSensitive 必须是布尔值");
      return { name, path: action.path, query: action.query, caseSensitive: action.caseSensitive === true };
    }
    if (["list_files", "read_file", "create_folder"].includes(name)) {
      if (typeof action.path !== "string") throw new Error("工具路径无效");
      if (name === "create_folder" && (!action.path.trim() || encoder.encode(action.path).length > 1024)) {
        throw new Error("文件夹路径不能为空，且最多为 1024 字节");
      }
      return { name, path: action.path };
    }
    if (name === "write_file") {
      if (typeof action.path !== "string" || typeof action.content !== "string") throw new Error("写入工具需要 path 和 content 文本字段");
      if (encoder.encode(action.content).length > 1024 * 1024) throw new Error("单个文件不能超过 1 MiB");
      return { name, path: action.path, content: action.content };
    }
    if (name === "edit_file") {
      if (typeof action.path !== "string" || typeof action.old_text !== "string" || typeof action.new_text !== "string") {
        throw new Error("编辑工具需要 path、old_text 和 new_text 文本字段");
      }
      if (!action.path.trim() || encoder.encode(action.path).length > 1024) throw new Error("编辑文件路径不能为空，且最多为 1024 字节");
      if (!action.old_text || encoder.encode(action.old_text).length > 1024 * 1024) throw new Error("old_text 不能为空且最多为 1 MiB");
      if (encoder.encode(action.new_text).length > 1024 * 1024) throw new Error("new_text 最多为 1 MiB");
      return { name, path: action.path, old_text: action.old_text, new_text: action.new_text };
    }
    if (name === "delete_file") {
      if (typeof action.path !== "string") throw new Error("删除工具路径无效");
      return { name, path: action.path };
    }
    if (name === "run_command") {
      if (typeof action.program !== "string" || !Array.isArray(action.args) || action.args.some((arg) => typeof arg !== "string")) {
        throw new Error("命令工具需要 program 文本和 args 字符串数组");
      }
      return { name, program: action.program, args: action.args };
    }
    throw new Error(`模型请求了未开放的工具：${name || "（空名称）"}`);
  }

  function createFolderDispatch(workspace, relativePath) {
    if (typeof workspace !== "string" || !workspace) throw new TypeError("workspace must be a non-empty string");
    if (typeof relativePath !== "string" || !relativePath.trim() || encoder.encode(relativePath).length > 1024) {
      throw new TypeError("relativePath must be a non-empty path of at most 1024 bytes");
    }
    return { command: "agent_create_folder", args: { workspace, relativePath } };
  }

  function editFileDispatch(workspace, action, approved = false) {
    if (typeof workspace !== "string" || !workspace) throw new TypeError("workspace must be a non-empty string");
    if (typeof approved !== "boolean") throw new TypeError("approved must be a boolean");
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
      },
    };
  }

  function preferredToolsForUserRequest(request) {
    const text = String(request ?? "");
    const folderRequested = /(?:创建|新建|建立|增加|新增|添加|加|建(?:一个|个)?)[\s\S]{0,48}(?:文件夹|目录)|(?:文件夹|目录)[\s\S]{0,48}(?:创建|新建|建立|增加|新增|添加)|\b(?:create|make|add|new)\b[\s\S]{0,64}\b(?:folder|directory)\b|\b(?:folder|directory)\b[\s\S]{0,64}\b(?:create|make|add|new)\b/i.test(text);
    const folderDenied = /(?:不要|别|无需|不需要|禁止)[\s\S]{0,16}(?:创建|新建|建立|增加|新增|添加|加)[\s\S]{0,48}(?:文件夹|目录)|\b(?:do not|don't|avoid|without)\b[\s\S]{0,64}\b(?:create|make|add|new)\b[\s\S]{0,64}\b(?:folder|directory)\b/i.test(text);
    const editVerb = /(?:修改|编辑|更改|改动|改写|改|更新|修复|替换|补充)|\b(?:edit|modify|change|update|fix|rewrite|replace)\b/i.test(text);
    const fileTarget = /(?:现有|已有)?(?:工作区)?(?:文件|文档|脚本|代码)|\b(?:file|document|script|code)\b|\b[\w.-]+\.(?:md|txt|rs|js|json|toml|yaml|yml|css|html|py|ts|tsx|jsx|sh|bat|c|cpp|h|hpp|go|java|kt|swift)\b/i.test(text);
    const editDenied = /(?:不要|别|无需|不需要|不用|禁止)[\s\S]{0,16}(?:修改|编辑|更改|改动|改写|改|更新|修复|替换)|\b(?:do not|don't|avoid|without)\b[\s\S]{0,32}\b(?:edit|modify|change|update|fix|rewrite|replace)\b/i.test(text);
    const tools = [];
    if (folderRequested && !folderDenied) tools.push("create_folder");
    if (editVerb && fileTarget && !editDenied) tools.push("edit_file");
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
    const valid = steps.length >= 1
      && steps.length === parsed.steps.length
      && steps.every((step) => encoder.encode(step).length <= 500);
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

  return { CONTINUATION_PREFIX, TOOL_RESULT_PREFIX, TOOL_ROUTING_GUIDANCE, appendContinuation, appendToolResult, parseAgentJson, appendMalformedResponseRetry, parseToolAction, createFolderDispatch, editFileDispatch, preferredToolsForUserRequest, appendDuplicatePlanRetry, validatePlan, recordPlanResponse, removeDuplicatePlanEntries };
});
