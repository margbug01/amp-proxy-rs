// Synthetic fixture generator for translator benchmarks. Run once with `node generate.js`.
// Targets: small ~5KB Anthropic Messages, medium ~50KB Gemini, large ~150KB OpenAI Responses.

const fs = require("fs");
const path = require("path");

function lorem(words) {
  const w = ["the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog",
    "function", "value", "context", "request", "response", "tool", "model",
    "json", "translator", "amp", "proxy", "deepseek", "openai", "claude", "gemini",
    "stream", "chunk", "token", "message", "system", "assistant", "user",
    "completion", "reasoning", "encrypted", "metadata", "param", "result"];
  const out = [];
  for (let i = 0; i < words; i++) out.push(w[(i * 13 + 7) % w.length]);
  return out.join(" ");
}

// ---------- small.json: Anthropic Messages ~5 KB ----------
const small = {
  model: "claude-sonnet-4-6",
  max_tokens: 8192,
  stream: true,
  system: [
    { type: "text", text: lorem(40) },
  ],
  messages: [
    { role: "user", content: [{ type: "text", text: lorem(80) }] },
    { role: "assistant", content: [{ type: "text", text: lorem(60) }] },
    { role: "user", content: [{ type: "text", text: lorem(120) }] },
    {
      role: "assistant",
      content: [
        { type: "text", text: lorem(40) },
        {
          type: "tool_use",
          id: "toolu_01abc",
          name: "read_file",
          input: { path: "/tmp/example.txt", offset: 0, limit: 100 },
        },
      ],
    },
    {
      role: "user",
      content: [
        {
          type: "tool_result",
          tool_use_id: "toolu_01abc",
          content: lorem(100),
        },
      ],
    },
  ],
  tools: [
    {
      name: "read_file",
      description: lorem(30),
      input_schema: {
        type: "object",
        properties: {
          path: { type: "string", description: "absolute path" },
          offset: { type: "integer" },
          limit: { type: "integer" },
        },
        required: ["path"],
      },
    },
    {
      name: "write_file",
      description: lorem(20),
      input_schema: {
        type: "object",
        properties: {
          path: { type: "string" },
          content: { type: "string" },
        },
        required: ["path", "content"],
      },
    },
  ],
};

// ---------- medium.json: Gemini generateContent ~50 KB ----------
const geminiContents = [];
for (let i = 0; i < 22; i++) {
  geminiContents.push({
    role: i % 2 === 0 ? "user" : "model",
    parts: [
      { text: lorem(240) },
      ...(i % 3 === 2
        ? [{ functionCall: { name: "search", args: { query: lorem(20), limit: 10 } } }]
        : []),
    ],
  });
}
const geminiTools = [
  {
    functionDeclarations: Array.from({ length: 10 }).map((_, k) => ({
      name: `tool_${k}`,
      description: lorem(40),
      parameters: {
        type: "object",
        properties: {
          query: { type: "string", description: lorem(15) },
          limit: { type: "integer", description: lorem(10) },
          filters: {
            type: "array",
            items: { type: "string" },
            description: lorem(10),
          },
          metadata: {
            type: "object",
            properties: {
              user: { type: "string" },
              session: { type: "string" },
              tags: { type: "array", items: { type: "string" } },
            },
          },
        },
        required: ["query"],
      },
    })),
  },
];
const medium = {
  systemInstruction: { parts: [{ text: lorem(100) }] },
  contents: geminiContents,
  tools: geminiTools,
  generationConfig: {
    temperature: 0.7,
    topP: 0.95,
    maxOutputTokens: 8192,
  },
};

// ---------- large.json: OpenAI Responses ~150 KB ----------
// Shape: model, stream, reasoning, tools, input array of system/message/reasoning/function_call/function_call_output.
const largeInput = [];
largeInput.push({ type: "system", role: "system", content: lorem(200) });

for (let i = 0; i < 28; i++) {
  // Alternate user message / assistant message / sometimes reasoning + function_call + function_call_output triples.
  largeInput.push({
    type: "message",
    role: i % 2 === 0 ? "user" : "assistant",
    content: [
      { type: i % 2 === 0 ? "input_text" : "output_text", text: lorem(420) },
    ],
  });
  if (i % 4 === 3) {
    largeInput.push({
      type: "reasoning",
      summary: [
        { type: "summary_text", text: lorem(200) },
        { type: "summary_text", text: lorem(180) },
      ],
      encrypted_content: lorem(120),
    });
    largeInput.push({
      type: "function_call",
      call_id: `call_${i}_a`,
      name: "read_file",
      arguments: JSON.stringify({ path: `/tmp/file_${i}.txt`, offset: 0, limit: 100 }),
    });
    largeInput.push({
      type: "function_call_output",
      call_id: `call_${i}_a`,
      output: lorem(380),
    });
  }
}

// Add an extra cluster of 5 tool calls late in the conversation.
for (let k = 0; k < 5; k++) {
  largeInput.push({
    type: "function_call",
    call_id: `call_late_${k}`,
    name: ["read_file", "write_file", "grep_files", "list_dir", "edit_file"][k],
    arguments: JSON.stringify({
      path: `/tmp/late_${k}.txt`,
      content: lorem(20),
      pattern: lorem(8),
    }),
  });
  largeInput.push({
    type: "function_call_output",
    call_id: `call_late_${k}`,
    output: lorem(120),
  });
}

const largeTools = Array.from({ length: 8 }).map((_, k) => ({
  type: "function",
  name: ["read_file", "write_file", "grep_files", "list_dir", "edit_file", "run_bash", "glob_files", "fetch_url"][k],
  description: lorem(40),
  parameters: {
    type: "object",
    properties: {
      path: { type: "string", description: lorem(12) },
      pattern: { type: "string", description: lorem(10) },
      offset: { type: "integer" },
      limit: { type: "integer" },
      content: { type: "string", description: lorem(15) },
    },
    required: ["path"],
  },
  strict: false,
}));

const large = {
  model: "gpt-5.4",
  stream: true,
  parallel_tool_calls: true,
  max_output_tokens: 16384,
  reasoning: { effort: "high", summary: "auto" },
  store: false,
  prompt_cache_key: "thread_abc123",
  include: ["reasoning.encrypted_content"],
  tools: largeTools,
  input: largeInput,
};

function write(name, obj) {
  const p = path.join(__dirname, name);
  const data = JSON.stringify(obj);
  fs.writeFileSync(p, data);
  return { name, bytes: data.length };
}

const r = [
  write("small.json", small),
  write("medium.json", medium),
  write("large.json", large),
];
console.log(r);
