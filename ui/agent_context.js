(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.LocmAgentContext = api;
})(globalThis, function () {
  const CONTEXT_PRESETS = Object.freeze([8192, 16384, 32768, 65536]);
  const DEFAULT_AUTO_CONTEXT = 8192;
  const MIN_REPLY_TOKENS = 1024;
  const MAX_REPLY_TOKENS = 4096;
  const INPUT_BUDGET_SAFETY_FACTOR = 0.75;
  const MAX_MESSAGE_BYTES = 30 * 1024;
  const MAX_HISTORY_MESSAGES = 50;
  const OMITTED_HISTORY_NOTE = "[LocM 上下文提示：为控制本次请求大小，较早的对话或工具结果已省略；保留了最初任务目标和最近消息。被省略的操作不代表已完成。若缺少必要信息，请重新检查工作区。]";
  const CLIPPED_MESSAGE_NOTE = "\n…（消息过长，已截断）";
  const encoder = new TextEncoder();
  const decoder = new TextDecoder();
  function utf8Bytes(value) {
    return encoder.encode(String(value ?? "")).length;
  }
  function contextBudget(contextPreference) {
    const preference = String(contextPreference ?? "auto");
    const contextCapacity = CONTEXT_PRESETS.includes(Number(preference))
      ? Number(preference)
      : DEFAULT_AUTO_CONTEXT;
    const replyTokens = Math.min(
      MAX_REPLY_TOKENS,
      Math.max(MIN_REPLY_TOKENS, Math.floor(contextCapacity / 8)),
    );
    // This is a cautious byte-budget heuristic, not a token count: UTF-8 bytes,
    // tokenizer output, chat-template framing, and server accounting differ.
    const inputBytes = Math.floor(
      (contextCapacity - replyTokens) * INPUT_BUDGET_SAFETY_FACTOR,
    );
    return { contextCapacity, inputBytes, replyTokens };
  }
  function truncateUtf8(value, maxBytes, suffix = CLIPPED_MESSAGE_NOTE) {
    const text = String(value ?? "");
    const encoded = encoder.encode(text);
    const byteLimit = Math.max(0, Math.floor(Number(maxBytes) || 0));
    if (encoded.length <= byteLimit) return text;
    const suffixBytes = encoder.encode(suffix).length;
    if (byteLimit <= suffixBytes) {
      let end = byteLimit;
      while (end > 0 && (encoded[end] & 0xc0) === 0x80) end--;
      return decoder.decode(encoded.slice(0, end));
    }
    let end = byteLimit - suffixBytes;
    while (end > 0 && (encoded[end] & 0xc0) === 0x80) end--;
    return decoder.decode(encoded.slice(0, end)) + suffix;
  }
  function serializedBytes(messages) {
    return utf8Bytes(JSON.stringify(messages));
  }
  function retainHistory(turns, maxMessages = MAX_HISTORY_MESSAGES) {
    const messages = (Array.isArray(turns) ? turns : [])
      .filter((message) => message && ["user", "assistant"].includes(message.role));
    const limit = Math.max(0, Math.floor(maxMessages));
    if (messages.length <= limit) return messages;
    if (!limit) return [];
    const firstUser = messages.find((message) => message.role === "user");
    const recentLimit = firstUser ? limit - 1 : limit;
    const recent = recentLimit ? messages.slice(-recentLimit) : [];
    return firstUser && !recent.includes(firstUser) ? [firstUser, ...recent] : recent;
  }
  function buildMessages(systemPrompt, turns, maxBytes = contextBudget("auto").inputBytes) {
    const byteLimit = Math.max(0, Math.floor(Number(maxBytes) || 0));
    const systemMessage = { role: "system", content: String(systemPrompt ?? "") };
    const sourceHistory = (Array.isArray(turns) ? turns : [])
      .filter((message) => message && ["user", "assistant"].includes(message.role));
    const retainedHistory = retainHistory(sourceHistory);
    const history = retainedHistory
      .map((message) => ({
        role: message.role,
        content: truncateUtf8(message.content, MAX_MESSAGE_BYTES),
      }));
    const complete = [systemMessage, ...history];
    if (sourceHistory.length === retainedHistory.length && serializedBytes(complete) <= byteLimit) return complete;
    const firstUserIndex = history.findIndex((message) => message.role === "user");
    const firstUser = firstUserIndex >= 0 ? { ...history[firstUserIndex] } : null;
    const note = { role: "assistant", content: OMITTED_HISTORY_NOTE };
    const base = () => [systemMessage, ...(firstUser ? [firstUser] : []), note];
    // The initial user request is the task's anchor. If it alone leaves no room,
    // shorten only that copy in this model request; the saved task history is untouched.
    let anchored = base();
    while (serializedBytes(anchored) > byteLimit && firstUser?.content) {
      const overBy = serializedBytes(anchored) - byteLimit;
      const currentBytes = utf8Bytes(firstUser.content);
      const nextLimit = Math.max(0, currentBytes - overBy - 16);
      if (nextLimit >= currentBytes) break;
      firstUser.content = truncateUtf8(firstUser.content, nextLimit);
      anchored = base();
    }
    let recent = [];
    const startIndex = firstUserIndex >= 0 ? firstUserIndex + 1 : 0;
    for (let index = history.length - 1; index >= startIndex; index--) {
      const message = history[index];
      const candidateRecent = [message, ...recent];
      const candidate = [...anchored, ...candidateRecent];
      if (serializedBytes(candidate) <= byteLimit) {
        recent = candidateRecent;
        continue;
      }
      let low = 0;
      let high = utf8Bytes(message.content);
      let best = null;
      while (low <= high) {
        const middle = Math.ceil((low + high) / 2);
        const clipped = { ...message, content: truncateUtf8(message.content, middle) };
        if (serializedBytes([...anchored, clipped, ...recent]) <= byteLimit) {
          best = clipped;
          low = middle + 1;
        } else {
          high = middle - 1;
        }
      }
      if (best && best.content) recent = [best, ...recent];
      break;
    }
    let result = [...anchored, ...recent];
    // Keep the hard byte guard even for unusually small caller-provided budgets.
    while (serializedBytes(result) > byteLimit && recent.length) {
      recent.shift();
      result = [...anchored, ...recent];
    }
    return result;
  }
  return {
    CONTEXT_PRESETS,
    DEFAULT_AUTO_CONTEXT,
    MAX_MESSAGE_BYTES,
    contextBudget,
    buildMessages,
    retainHistory,
    serializedBytes,
    utf8Bytes,
  };
});
