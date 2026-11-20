const test = require("node:test");
const assert = require("node:assert/strict");
const { buildMessages } = require("./agent_context.js");
const { TOOL_RESULT_PREFIX, TOOL_ROUTING_GUIDANCE, TOOL_OPTIONS, DEFAULT_ENABLED_TOOLS, normalizeEnabledTools, normalizeScope, enabledToolsForScope, setAllToolsEnabled, masterToolSwitchState, appendContinuation, appendToolResult, parseAgentJson, parseMiniCpm5ToolCall, appendMalformedResponseRetry, parseToolAction, createFolderDispatch, editFileDispatch, preferredToolsForUserRequest, appendDuplicatePlanRetry, recordPlanResponse, removeDuplicatePlanEntries } = require("./agent_protocol.js");

test("Agent exposes ten independently configurable tools, with public-web tools disabled by default", () => {
  const all = normalizeEnabledTools();
  assert.equal(TOOL_OPTIONS.length, 10);
  assert.equal(all.length, 8);
  assert.deepEqual(all, DEFAULT_ENABLED_TOOLS);
  assert.ok(!all.includes("web_search"));
  assert.ok(!all.includes("web_fetch"));
  assert.deepEqual(normalizeEnabledTools([]), []);
  assert.deepEqual(normalizeEnabledTools(["read_file", "unknown", "create_folder"]), ["read_file", "create_folder"]);
  assert.equal(normalizeEnabledTools(["future_tool"]).length, 8);
  assert.throws(() => parseToolAction({ type: "tool_call", name: "read_file", path: "a.txt" }, ["list_files"]), /已关闭/);
  assert.deepEqual(parseToolAction({ type: "tool_call", name: "read_file", path: "a.txt" }, ["read_file"]), { name: "read_file", path: "a.txt" });
});

test("read-only scope removes side-effect tools before tool parsing", () => {
  const all = TOOL_OPTIONS.map((tool) => tool.id);
  assert.equal(normalizeScope("full"), "workspace");
  assert.equal(normalizeScope("read_only"), "read_only");
  assert.deepEqual(enabledToolsForScope(all, "read_only"), ["list_files", "search_files", "read_file", "web_search", "web_fetch"]);
  assert.deepEqual(enabledToolsForScope(undefined, "read_only"), ["list_files", "search_files", "read_file"]);
  assert.throws(() => parseToolAction({ type: "tool_call", name: "write_file", path: "a.txt", content: "x" }, enabledToolsForScope(all, "read_only")), /已关闭/);
});

test("web tools validate bounded search text and reject unsafe URL syntax", () => {
  assert.deepEqual(parseToolAction({ type: "tool_call", name: "web_search", query: "  public query  " }, ["web_search"]), { name: "web_search", query: "public query" });
  assert.deepEqual(parseToolAction({ type: "tool_call", name: "web_fetch", url: "https://example.com/path" }, ["web_fetch"]), { name: "web_fetch", url: "https://example.com/path" });
  assert.throws(() => parseToolAction({ type: "tool_call", name: "web_search", query: "" }, ["web_search"]), /不能为空/);
  assert.throws(() => parseToolAction({ type: "tool_call", name: "web_search", query: "q".repeat(257) }, ["web_search"]), /256 字节/);
  assert.throws(() => parseToolAction({ type: "tool_call", name: "web_fetch", url: "file:///etc/passwd" }, ["web_fetch"]), /HTTP\(S\)/);
  assert.throws(() => parseToolAction({ type: "tool_call", name: "web_fetch", url: "https://user:pass@example.com" }, ["web_fetch"]), /凭据/);
});

test("master tool switch enables or disables all tools and reports partial selection", () => {
  const all = TOOL_OPTIONS.map((tool) => tool.id);
  assert.deepEqual(setAllToolsEnabled(true), all);
  assert.deepEqual(setAllToolsEnabled(false), []);
  assert.deepEqual(masterToolSwitchState(all), { checked: true, indeterminate: false });
  assert.deepEqual(masterToolSwitchState([]), { checked: false, indeterminate: false });
  assert.deepEqual(masterToolSwitchState(["read_file"]), { checked: false, indeterminate: true });
});

test("a plan is recorded once and the next model request gets an explicit internal continuation", () => {
  const task = { needsPlan: true, plan: [], turns: [{ role: "user", content: "检查项目并修复问题" }] };
  const parsed = { type: "plan", steps: ["检查项目", "修复问题"] };
  const first = recordPlanResponse(task, JSON.stringify(parsed), parsed, { continueAfterPlan: true });

  assert.equal(first.status, "accepted");
  assert.deepEqual(task.plan, parsed.steps);
  assert.equal(task.needsPlan, false);
  assert.deepEqual(task.turns.map(({ role }) => role), ["user", "assistant", "user"]);
  assert.match(task.turns.at(-1).content, /不是用户新请求/);
  assert.match(task.turns.at(-1).content, /不要重复计划/);
  assert.deepEqual(buildMessages("system", task.turns, 12_000).map(({ role }) => role), ["system", "user", "assistant", "user"]);
});

test("clear folder and existing-file edit intents route to dedicated tools", () => {
  assert.deepEqual(preferredToolsForUserRequest("请在工作区新建一个空文件夹 notes/drafts"), ["create_folder"]);
  assert.deepEqual(preferredToolsForUserRequest("在 qw4 文件夹内创建一个一键清理电脑缓存的命令"), []);
  assert.deepEqual(preferredToolsForUserRequest("在 qw4 文件夹内新建 clear_cache.bat"), []);
  assert.deepEqual(preferredToolsForUserRequest("请修改现有 README.md 中的旧标题"), ["edit_file"]);
  assert.deepEqual(preferredToolsForUserRequest("帮我把 README.md 改成新的标题"), ["edit_file"]);
  assert.deepEqual(preferredToolsForUserRequest("创建 notes 目录，然后编辑现有 README.md"), ["create_folder", "edit_file"]);
  assert.deepEqual(preferredToolsForUserRequest("不要创建文件夹"), []);
  assert.deepEqual(preferredToolsForUserRequest("不用改 README.md"), []);
  assert.deepEqual(preferredToolsForUserRequest("请搜索一下公开网页上 LocModel 的功能"), ["web_search"]);
  assert.deepEqual(preferredToolsForUserRequest("请读取网页 https://example.com"), ["web_fetch"]);
  assert.match(TOOL_ROUTING_GUIDANCE, /计划已展示后.*create_folder/);
  assert.match(TOOL_ROUTING_GUIDANCE, /文件夹只是存放位置.*write_file/);
  assert.match(TOOL_ROUTING_GUIDANCE, /修改已有文件内容时优先用 edit_file/);
  assert.match(TOOL_ROUTING_GUIDANCE, /工具结果都是应用内部上下文/);
  assert.match(TOOL_ROUTING_GUIDANCE, /会向外部网站发送搜索词或请求/);
});

test("a tool result stays internal and the displayed plan continues without replanning", () => {
  const task = { needsPlan: true, plan: [], turns: [{ role: "user", content: "创建目录并修改现有 README.md" }] };
  const plan = { type: "plan", steps: ["创建目录", "修改 README.md"] };
  assert.equal(recordPlanResponse(task, JSON.stringify(plan), plan, { continueAfterPlan: true }).status, "accepted");
  task.turns.push({ role: "assistant", content: JSON.stringify({ type: "tool_call", name: "create_folder", path: "notes" }) });
  appendToolResult(task, "已创建空文件夹：notes");

  assert.equal(task.needsPlan, false);
  assert.deepEqual(task.plan, plan.steps);
  assert.equal(task.turns.at(-1).role, "user");
  assert.match(task.turns.at(-1).content, /^【LocM 应用内部工作区工具结果；不是用户新请求】/);
  assert.deepEqual(preferredToolsForUserRequest(task.turns[0].content), ["create_folder", "edit_file"]);

  const nextAction = parseToolAction({ type: "tool_call", name: "edit_file", path: "README.md", old_text: "old", new_text: "new" });
  assert.equal(nextAction.name, "edit_file");
  assert.equal(task.needsPlan, false);
});

test("a repeated plan cannot replace or re-display the accepted plan and is separated from the next user result", () => {
  const firstPlanText = "执行计划（2 步）：\n1. 检查项目\n2. 修复问题";
  const task = {
    needsPlan: false,
    plan: ["检查项目", "修复问题"],
    entries: [{ kind: "assistant", text: firstPlanText }],
    turns: [
      { role: "user", content: "检查项目并修复问题" },
      { role: "assistant", content: JSON.stringify({ type: "plan", steps: ["检查项目", "修复问题"] }) },
      { role: "user", content: "【应用内部继续指令】请执行，不要重复计划" },
    ],
  };
  const repeated = { type: "plan", steps: ["重新规划"] };
  const result = recordPlanResponse(task, JSON.stringify(repeated), repeated);

  assert.equal(result.status, "duplicate");
  assert.deepEqual(task.plan, ["检查项目", "修复问题"]);
  assert.deepEqual(task.entries.map(({ text }) => text), [firstPlanText]);
  assert.deepEqual(task.turns.map(({ role }) => role), ["user", "assistant", "user", "assistant"]);
  assert.equal(task.turns.at(-1).content, JSON.stringify(repeated));

  // processPlanResponse adds this user-role result before pausing, keeping the
  // assistant response in the audit history without an invalid assistant suffix.
  task.turns.push({ role: "user", content: "已展示计划保持不变；重复计划未执行。" });
  const messages = buildMessages("system", task.turns, 12_000);
  assert.deepEqual(messages.map(({ role }) => role), ["system", "user", "assistant", "user", "assistant", "user"]);
  assert.equal(removeDuplicatePlanEntries([
    { kind: "assistant", text: firstPlanText },
    { kind: "assistant", text: firstPlanText },
  ]).length, 1);
});

test("a genuine new user request permits a fresh plan, while recovery can explicitly replan", () => {
  const task = { needsPlan: true, plan: ["old"], turns: [{ role: "user", content: "新的用户指示" }] };
  const parsed = { type: "plan", steps: ["新计划"] };
  assert.equal(recordPlanResponse(task, JSON.stringify(parsed), parsed).status, "accepted");
  assert.deepEqual(task.plan, ["新计划"]);

  task.needsPlan = false;
  const recovery = { type: "plan", steps: ["恢复时检查状态"] };
  assert.equal(recordPlanResponse(task, JSON.stringify(recovery), recovery, { allowReplan: true }).status, "accepted");
  assert.deepEqual(task.plan, recovery.steps);
});

test("continuations are user-role boundaries, never a second trailing assistant", () => {
  const task = { turns: [{ role: "assistant", content: "进度" }] };
  appendContinuation(task, "继续。");
  assert.deepEqual(task.turns.map(({ role }) => role), ["assistant", "user"]);
});

test("a duplicate plan gets at most one explicit correction continuation", () => {
  const task = {
    turns: [
      { role: "user", content: "检查工作区" },
      { role: "assistant", content: '{"type":"plan","steps":["列出文件"]}' },
      { role: "user", content: "【LocM 应用内部继续指令；不是用户新请求】执行计划" },
      { role: "assistant", content: '{"type":"plan","steps":["重新列计划"]}' },
    ],
  };

  const retry = appendDuplicatePlanRetry(task, 1);
  assert.deepEqual(retry, { status: "retry", retriesRemaining: 0 });
  assert.equal(task.turns.at(-1).role, "user");
  assert.match(task.turns.at(-1).content, /^【LocM 应用内部继续指令；不是用户新请求】/);
  assert.match(task.turns.at(-1).content, /不要重新生成或修改计划/);
  assert.match(task.turns.at(-1).content, /list_files/);
  const countBeforeExhaustedRetry = task.turns.length;
  assert.deepEqual(appendDuplicatePlanRetry(task, retry.retriesRemaining), { status: "exhausted", retriesRemaining: 0 });
  assert.equal(task.turns.length, countBeforeExhaustedRetry);
  assert.equal(TOOL_RESULT_PREFIX, "【LocM 应用内部工作区工具结果；不是用户新请求】");
});

test("Agent JSON parsing accepts one strict or fenced protocol object and handles braces in strings", () => {
  const action = { type: "tool_call", name: "write_file", path: "notes/todo.txt", content: "keep {these} braces" };
  assert.deepEqual(parseAgentJson(JSON.stringify(action)), action);
  assert.deepEqual(parseAgentJson(`模型回复：\n\`\`\`json\n${JSON.stringify(action)}\n\`\`\`\n结束。`), action);
  assert.deepEqual(parseAgentJson(`我会继续。 ${JSON.stringify(action)} 已准备。`), action);
});

test("Agent JSON parsing repairs only the observed incomplete plan suffix", () => {
  const malformed = '{"type":"plan","steps":["检查工作区文件内容","询问是否需要处理现有文件内容","确认任务优先级"})';
  const expected = { type: "plan", steps: ["检查工作区文件内容", "询问是否需要处理现有文件内容", "确认任务优先级"] };

  assert.deepEqual(parseAgentJson(malformed), expected);
  assert.deepEqual(parseAgentJson(`\`\`\`json\n${malformed}\n\`\`\``), expected);
  assert.equal(parseAgentJson('{"type":"tool_call","name":"list_files","path":"."'), null);
});

test("Agent JSON parsing rejects ordinary prose, arrays, malformed JSON, and ambiguous objects", () => {
  assert.equal(parseAgentJson("好的，我现在创建文件夹。"), null);
  assert.equal(parseAgentJson("mkdir -p notes/drafts"), null);
  assert.equal(parseAgentJson("[ {\"type\":\"tool_call\",\"name\":\"run_command\"} ]"), null);
  assert.equal(parseAgentJson("{\"type\":\"tool_call\",}"), null);
  assert.equal(parseAgentJson("首选 {\"type\":\"message\",\"message\":\"等一下\"}；备选 {\"type\":\"done\",\"message\":\"完成\"}"), null);
  assert.equal(parseAgentJson("说明文字：\"{\\\"type\\\":\\\"tool_call\\\",\\\"name\\\":\\\"run_command\\\"}\""), null);
});

test("MiniCPM5 XML function calls normalize only supported tools and declared parameters", () => {
  assert.deepEqual(parseAgentJson('<function name="read_file"><param name="path">README.md</param></function>'), {
    type: "tool_call", name: "read_file", path: "README.md",
  });
  assert.deepEqual(parseMiniCpm5ToolCall('先检查文件。<function name="search_files"><param name="path">.</param><param name="query">TODO &amp; FIXME</param><param name="caseSensitive">true</param></function>'), {
    name: "search_files", path: ".", query: "TODO & FIXME", caseSensitive: true,
  });
  assert.deepEqual(parseAgentJson('<function name="write_file"><param name="path">notes/todo.txt</param><param name="content"><![CDATA[line 1\nline 2]]></param></function>'), {
    type: "tool_call", name: "write_file", path: "notes/todo.txt", content: "line 1\nline 2",
  });
  assert.deepEqual(parseAgentJson('<function name="run_command"><param name="program">git</param><param name="args">["status","--short"]</param></function>'), {
    type: "tool_call", name: "run_command", program: "git", args: ["status", "--short"],
  });
});

test("MiniCPM5 XML parser rejects unknown, multiple, incomplete, duplicate, and extra-argument calls", () => {
  assert.equal(parseMiniCpm5ToolCall('<function name="run_shell"><param name="command">whoami</param></function>'), null);
  assert.equal(parseMiniCpm5ToolCall('<function name="read_file"><param name="path">a</param><param name="path">b</param></function>'), null);
  assert.equal(parseMiniCpm5ToolCall('<function name="read_file"><param name="path">a</param><param name="outside">b</param></function>'), null);
  assert.equal(parseMiniCpm5ToolCall('<function name="read_file"><param name="query">x</param></function>'), null);
  assert.equal(parseMiniCpm5ToolCall('<function name="search_files"><param name="path">.</param><param name="query">TODO</param><param name="caseSensitive">yes</param></function>'), null);
  assert.equal(parseMiniCpm5ToolCall('<function name="read_file"><param name="path">a</param></function><function name="read_file"><param name="path">b</param></function>'), null);
  assert.equal(parseMiniCpm5ToolCall('<function name="read_file"><param>bad schema</param></function>'), null);
  assert.deepEqual(parseAgentJson('<function name="delete_file"><param name="path">README.md</param></function>'), {
    type: "tool_call", name: "delete_file", path: "README.md",
  });
});

test("malformed Agent output gets one internal correction retry, then safely exhausts", () => {
  const task = { turns: [{ role: "user", content: "创建 notes 文件夹" }] };
  const retry = appendMalformedResponseRetry(task, 1);
  assert.deepEqual(retry, { status: "retry", retriesRemaining: 0 });
  assert.equal(task.turns.at(-1).role, "user");
  assert.match(task.turns.at(-1).content, /^【LocM 应用内部继续指令；不是用户新请求】/);
  assert.match(task.turns.at(-1).content, /标准函数工具/);
  assert.match(task.turns.at(-1).content, /JSON tool_call对象/);
  assert.match(task.turns.at(-1).content, /MiniCPM5/);
  assert.match(task.turns.at(-1).content, /不要把历史中的自然语言.*改写成可执行命令/);
  assert.match(task.turns.at(-1).content, /仍需应用确认卡和系统原生确认/);
  const countBeforeExhaustedRetry = task.turns.length;
  assert.deepEqual(appendMalformedResponseRetry(task, retry.retriesRemaining), { status: "exhausted", retriesRemaining: 0 });
  assert.equal(task.turns.length, countBeforeExhaustedRetry);
});

test("create_folder schema dispatches only to the native workspace command", () => {
  const action = parseToolAction({ type: "tool_call", name: "create_folder", path: "notes/drafts" });
  const permissionToken = "a".repeat(64);
  assert.deepEqual(action, { name: "create_folder", path: "notes/drafts" });
  assert.deepEqual(createFolderDispatch("/workspace/project", action.path, "task-a", permissionToken), {
    command: "agent_create_folder",
    args: { workspace: "/workspace/project", relativePath: "notes/drafts", taskId: "task-a", permissionToken },
  });
  assert.notEqual(createFolderDispatch("/workspace/project", action.path, "task-a", permissionToken).command, "agent_run_command");
  assert.throws(() => parseToolAction({ type: "tool_call", name: "create_folder", path: "" }), /不能为空/);
  assert.throws(() => createFolderDispatch("/workspace/project", " ", "task-a", permissionToken), /relativePath/);
  assert.throws(() => createFolderDispatch("/workspace/project", action.path, "" , permissionToken), /taskId/);
  assert.throws(() => createFolderDispatch("/workspace/project", action.path, "task-a", ""), /permissionToken/);
  assert.equal(parseAgentJson("mkdir -p notes/drafts"), null);
});

test("edit_file requires exact old/new snippets and dispatches with approval disabled until approved", () => {
  const action = parseToolAction({ type: "tool_call", name: "edit_file", path: "docs/README.md", old_text: "old title", new_text: "new title" });
  const permissionToken = "b".repeat(64);
  assert.deepEqual(action, { name: "edit_file", path: "docs/README.md", old_text: "old title", new_text: "new title" });
  assert.deepEqual(editFileDispatch("/workspace/project", action, false, "task-a", permissionToken), {
    command: "agent_edit_file",
    args: { workspace: "/workspace/project", relativePath: "docs/README.md", oldText: "old title", newText: "new title", allowEdit: false, taskId: "task-a", permissionToken },
  });
  assert.deepEqual(editFileDispatch("/workspace/project", action, true, "task-a", permissionToken).args, {
    workspace: "/workspace/project", relativePath: "docs/README.md", oldText: "old title", newText: "new title", allowEdit: true, taskId: "task-a", permissionToken,
  });
  assert.throws(() => editFileDispatch("/workspace/project", action, false, "" , permissionToken), /taskId/);
  assert.throws(() => editFileDispatch("/workspace/project", action, false, "task-a", ""), /permissionToken/);
  assert.throws(() => parseToolAction({ type: "tool_call", name: "edit_file", path: "docs/README.md", old_text: "", new_text: "x" }), /old_text 不能为空/);
  assert.throws(() => parseToolAction({ type: "tool_call", name: "edit_file", path: "docs/README.md", old_text: "old" }), /old_text 和 new_text/);
  assert.notEqual(editFileDispatch("/workspace/project", action, false, "task-a", permissionToken).command, "agent_run_command");
});

test("loading historical task entries collapses only identical adjacent plan cards", () => {
  const plan = "执行计划（1 步）：\n1. 检查文件";
  const entries = [
    { kind: "assistant", text: plan },
    { kind: "assistant", text: plan },
    { kind: "tool", text: "工具结果" },
    { kind: "assistant", text: plan },
    { kind: "assistant", text: `${plan}\n不同计划` },
  ];
  const cleaned = removeDuplicatePlanEntries(entries);

  assert.deepEqual(cleaned.map(({ kind, text }) => [kind, text]), [
    ["assistant", plan],
    ["tool", "工具结果"],
    ["assistant", plan],
    ["assistant", `${plan}\n不同计划`],
  ]);
});
