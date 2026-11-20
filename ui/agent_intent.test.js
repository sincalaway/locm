const test = require("node:test");
const assert = require("node:assert/strict");
const { directReply, createFolderRequest } = require("./agent_intent.js");

test("answers a simple Chinese identity question without a model or workspace", () => {
  const reply = directReply("你是谁？");
  assert.match(reply, /LocM 本地工作区 Agent/);
  assert.match(reply, /先请求确认/);
});

test("answers English identity, capability, and greeting prompts directly", () => {
  assert.match(directReply("Who are you?"), /LocM's local workspace agent/);
  assert.match(directReply("What can you do?"), /read, search, and create files/);
  assert.match(directReply("Hello!"), /What can I help you with/);
});

test("does not bypass planning for workspace requests or attached-file tasks", () => {
  assert.equal(directReply("你是谁？请读取 README.md"), null);
  assert.equal(directReply("你是谁？", true), null);
  assert.equal(directReply("帮我检查这个项目"), null);
  assert.equal(directReply(""), null);
});

test("recognizes explicit folder creation with a target path", () => {
  assert.deepEqual(createFolderRequest("帮我创建一个ewoe文件夹"), { path: "ewoe", simple: true });
  assert.deepEqual(createFolderRequest("请在工作区新建一个空文件夹 notes/drafts"), { path: "notes/drafts", simple: true });
  assert.deepEqual(createFolderRequest("创建一个名为“项目-一”的目录"), { path: "项目-一", simple: true });
  assert.deepEqual(createFolderRequest("Create a folder called notes"), { path: "notes", simple: true });
  assert.deepEqual(createFolderRequest("Create notes folder"), { path: "notes", simple: true });
  assert.deepEqual(createFolderRequest("帮我创建ewoe文件夹，然后列出目录"), { path: "ewoe", simple: false });
});

test("does not turn negative, informational, pathless, or attached-file requests into folder operations", () => {
  assert.equal(createFolderRequest("不要创建ewoe文件夹"), null);
  assert.equal(createFolderRequest("我不想创建ewoe文件夹"), null);
  assert.equal(createFolderRequest("怎么创建ewoe文件夹？"), null);
  assert.equal(createFolderRequest("帮我创建一个文件夹"), null);
  assert.equal(createFolderRequest("帮我创建一个ewoe文件夹", true), null);
});
