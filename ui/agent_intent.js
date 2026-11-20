(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.LocmAgentIntent = api;
})(globalThis, function () {
  const IDENTITY_PATTERNS = [
    /^(?:你是谁|你是(?:谁|什么)|你是什么(?:助手|模型|人工智能)?|请问你是谁|who are you|what are you|what kind of assistant are you|are you an ai(?: assistant)?)$/iu,
  ];
  const CAPABILITY_PATTERNS = [
    /^(?:你能做什么|你会做什么|你可以做什么|你有什么能力|what can you do|what are your capabilities)$/iu,
  ];
  const GREETING_PATTERNS = [
    /^(?:你好|您好|嗨|哈喽|早上好|晚上好|hello|hi|hey|good morning|good afternoon|good evening)$/iu,
  ];

  function normalize(text) {
    return String(text ?? "")
      .trim()
      .replace(/[\s，。！？!?、,.~～…]+$/gu, "")
      .replace(/\s+/gu, " ");
  }

  function createFolderRequest(text, hasAttachments = false) {
    if (hasAttachments) return null;
    const prompt = normalize(text);
    if (!prompt || prompt.length > 1000) return null;
    const nonAction = /(?:不要|别|无需|不需要|不想|禁止|避免|不能)[\s\S]{0,16}(?:创建|新建|建立|增加|新增|添加|建)[\s\S]{0,48}(?:文件夹|目录)|(?:如何|怎么|怎样|能否|可不可以|是否).{0,32}(?:创建|新建|建立).{0,32}(?:文件夹|目录)|\b(?:how to|how can i|can i|should i)\b[\s\S]{0,64}\b(?:create|make|add)\b[\s\S]{0,32}\b(?:folder|directory)\b/i;
    if (nonAction.test(prompt)) return null;

    const patterns = [
      /(?:创建|新建|建立|新增|增加|添加|建)(?:一个|个)?\s*(?:(?:名为|叫做|名叫|叫)\s*)?["'“「『]?([^\s"'“”「」『』，,。；;]+)["'”」』]?\s*(?:的\s*)?(?:空的?\s*)?(?:文件夹|目录)/u,
      /(?:创建|新建|建立|新增|增加|添加|建)(?:一个|个)?\s*(?:空的?\s*)?(?:文件夹|目录)\s*(?:(?:名为|叫做|名叫|叫)\s*)?["'“「『]?([^\s"'“”「」『』，,。；;]+)["'”」』]?/u,
      /\b(?:create|make|add|new)\s+(?:a\s+)?(?:folder|directory)\s+(?:named|called)\s+([^\s"'“”「」『』,;]+)/i,
      /\b(?:create|make|add|new)\s+(?:a\s+)?([^\s"'“”「」『』,;]+)\s+(?:folder|directory)\b/i,
    ];
    for (const pattern of patterns) {
      const match = prompt.match(pattern);
      const path = match?.[1]?.replace(/^[`“「『"']+|[`”」』"']+$/gu, "");
      if (path && ![".", "..", "一个", "个", "空", "空的"].includes(path)) {
        const prefix = prompt.slice(0, match.index)
          .replace(/(?:请|帮我|麻烦|在工作区|在当前工作区|在工作区根目录下|在当前工作区根目录下|please|in the workspace|in workspace)/giu, "")
          .trim();
        const suffix = prompt.slice(match.index + match[0].length)
          .replace(/(?:吧|即可|就行|please|now|thanks|thank you|[。！!,.?])/giu, "")
          .trim();
        return { path, simple: !prefix && !suffix };
      }
    }
    return null;
  }

  function directReply(text, hasAttachments = false) {
    if (hasAttachments) return null;
    const prompt = normalize(text);
    if (!prompt || prompt.length > 100) return null;
    if (IDENTITY_PATTERNS.some((pattern) => pattern.test(prompt))) {
      return /[\u3400-\u9fff]/u.test(prompt)
        ? "我是 LocM 本地工作区 Agent，使用你在“运行”页面启动的本地模型回答问题。选择工作文件夹后，我可以查看、搜索和新建文件；覆盖、删除或运行命令前会先请求确认。"
        : "I'm LocM's local workspace agent. I use the local model you start on the Run page. With a workspace selected, I can read, search, and create files; overwriting, deleting, or running commands requires your confirmation first.";
    }
    if (CAPABILITY_PATTERNS.some((pattern) => pattern.test(prompt))) {
      return /[\u3400-\u9fff]/u.test(prompt)
        ? "我可以直接回答简单问题；选择工作文件夹并启动本地模型后，也能协助读取、搜索和新建文件。覆盖、删除文件或运行命令前会先请求确认。"
        : "I can answer simple questions directly. With a workspace selected and a local model running, I can also read, search, and create files. I'll ask for confirmation before overwriting or deleting files, or running commands.";
    }
    if (GREETING_PATTERNS.some((pattern) => pattern.test(prompt))) {
      return /[\u3400-\u9fff]/u.test(prompt)
        ? "你好！我是 LocM 本地工作区 Agent。有什么我可以帮你？"
        : "Hello! I'm LocM's local workspace agent. What can I help you with?";
    }
    return null;
  }

  return { directReply, createFolderRequest };
});
