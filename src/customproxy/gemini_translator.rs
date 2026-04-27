//! Gemini ↔ OpenAI Responses translator.
//!
//! Ported from `internal/customproxy/gemini_translator.go`. Drops Gemini-only
//! `thoughtSignature` blobs, synthesizes `call_id` values for paired
//! `functionCall`/`functionResponse` items (Gemini has no such field),
//! normalizes Gemini's UPPERCASE schema `type` keywords to JSON Schema's
//! lowercase, and threads any thinking suffix on the resolved model name into
//! OpenAI's `reasoning.effort` field.
//!
//! The Go version uses `context.WithValue` to ferry per-request state from
//! the request phase into `ModifyResponse`. In Rust this struct is a plain
//! value the request handler stores in axum extensions and the response
//! handler retrieves; the translator functions themselves are pure.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};

use crate::thinking::parse_suffix;

/// Per-request translator state carried from request to response phase.
#[derive(Debug, Clone, Default)]
pub struct GeminiTranslateCtx {
    /// The Gemini model name from the original request URL path. Echoed
    /// back into the translated response's `modelVersion` field so Amp CLI
    /// logs stay coherent.
    pub original_model: String,
}

impl GeminiTranslateCtx {
    /// Builds a context tagged with the originally requested Gemini model.
    pub fn new(original_model: impl Into<String>) -> Self {
        Self {
            original_model: original_model.into(),
        }
    }
}

/// Converts a Gemini v1beta1 `generateContent` request body into the
/// equivalent OpenAI Responses API body. `mapped_model` is the resolved
/// custom-provider target — any thinking suffix is stripped from the
/// forwarded model name and turned into `reasoning.effort` instead.
pub fn translate_gemini_request_to_openai(body: &[u8], mapped_model: &str) -> Result<Vec<u8>> {
    if body.is_empty() {
        return Err(anyhow!("gemini translate: empty request body"));
    }
    let req: Value = serde_json::from_slice(body).context("gemini translate: parse request")?;
    let req_obj = req
        .as_object()
        .ok_or_else(|| anyhow!("gemini translate: request body must be a JSON object"))?;

    let suffix = parse_suffix(mapped_model);
    let openai_model = suffix.model_name.clone();
    let reasoning_effort = if suffix.has_suffix {
        suffix.effort.clone()
    } else {
        None
    };

    let mut input: Vec<Value> = Vec::new();

    // systemInstruction → system input item.
    if let Some(si) = req_obj.get("systemInstruction").and_then(|v| v.as_object()) {
        let sys = collect_gemini_text(si.get("parts"));
        if !sys.is_empty() {
            input.push(json!({
                "role": "system",
                "content": sys,
            }));
        }
    }

    let empty: Vec<Value> = Vec::new();
    let contents = req_obj
        .get("contents")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    let mut pending_call_ids: Vec<String> = Vec::new();
    let mut call_id_counter: u32 = 0;

    for content in contents {
        let Some(content_obj) = content.as_object() else {
            continue;
        };
        let role = content_obj
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let parts = content_obj
            .get("parts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        match role {
            "user" => {
                append_user_content(&mut input, &parts, &pending_call_ids);
                pending_call_ids.clear();
            }
            "model" => {
                pending_call_ids = append_model_content(&mut input, &parts, &mut call_id_counter);
            }
            _ => {
                let txt = collect_gemini_text(Some(&Value::Array(parts.clone())));
                if !txt.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": txt}],
                    }));
                }
            }
        }
    }

    // Tools: flatten Gemini functionDeclarations into Responses-shape tools.
    let mut tools: Vec<Value> = Vec::new();
    if let Some(raw_tools) = req_obj.get("tools").and_then(|v| v.as_array()) {
        for rt in raw_tools {
            let Some(group) = rt.as_object() else {
                continue;
            };
            let Some(decls) = group.get("functionDeclarations").and_then(|v| v.as_array()) else {
                continue;
            };
            for d in decls {
                let Some(decl) = d.as_object() else { continue };
                let mut tool = Map::new();
                tool.insert("type".into(), Value::String("function".into()));
                if let Some(name) = decl.get("name") {
                    tool.insert("name".into(), name.clone());
                }
                tool.insert("strict".into(), Value::Bool(false));
                if let Some(desc) = decl.get("description").and_then(|v| v.as_str()) {
                    if !desc.is_empty() {
                        tool.insert("description".into(), Value::String(desc.into()));
                    }
                }
                let params = match decl.get("parameters") {
                    Some(p) if !p.is_null() => normalize_schema_type_case(p.clone()),
                    _ => json!({"type": "object", "properties": {}}),
                };
                tool.insert("parameters".into(), params);
                tools.push(Value::Object(tool));
            }
        }
    }

    let mut out = Map::new();
    out.insert("model".into(), Value::String(openai_model));
    out.insert("input".into(), Value::Array(input));
    out.insert("stream".into(), Value::Bool(true));
    out.insert("store".into(), Value::Bool(false));
    out.insert("parallel_tool_calls".into(), Value::Bool(true));
    out.insert(
        "include".into(),
        Value::Array(vec![Value::String("reasoning.encrypted_content".into())]),
    );
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
    }

    if let Some(gc) = req_obj.get("generationConfig").and_then(|v| v.as_object()) {
        if let Some(mot) = gc.get("maxOutputTokens").and_then(|v| v.as_i64()) {
            if mot > 0 {
                out.insert("max_output_tokens".into(), Value::from(mot));
            }
        }
    }

    if let Some(eff) = reasoning_effort {
        out.insert(
            "reasoning".into(),
            json!({"effort": eff, "summary": "auto"}),
        );
    }

    serde_json::to_vec(&Value::Object(out)).context("gemini translate: marshal request")
}

/// Concatenates every `text` field found inside a Gemini parts array,
/// joining separate parts with `"\n\n"`.
fn collect_gemini_text(raw: Option<&Value>) -> String {
    let Some(parts) = raw.and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut buf = String::new();
    for p in parts {
        let Some(part) = p.as_object() else { continue };
        let Some(txt) = part.get("text").and_then(|v| v.as_str()) else {
            continue;
        };
        if txt.is_empty() {
            continue;
        }
        if !buf.is_empty() {
            buf.push_str("\n\n");
        }
        buf.push_str(txt);
    }
    buf
}

/// Flattens a Gemini user-role parts array into Responses input items. Plain
/// text parts collapse into a single `message`; functionResponse parts emit
/// one `function_call_output` each, aligned positionally to the
/// `pending_call_ids` produced by the immediately preceding model turn.
fn append_user_content(input: &mut Vec<Value>, parts: &[Value], pending_call_ids: &[String]) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut func_responses: Vec<(String, Value)> = Vec::new();

    for p in parts {
        let Some(part) = p.as_object() else { continue };
        if let Some(txt) = part.get("text").and_then(|v| v.as_str()) {
            if !txt.is_empty() {
                text_parts.push(txt.to_string());
                continue;
            }
        }
        if let Some(fr) = part.get("functionResponse").and_then(|v| v.as_object()) {
            let name = fr
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let resp = fr
                .get("response")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            func_responses.push((name, resp));
        }
    }

    if !text_parts.is_empty() {
        let content: Vec<Value> = text_parts
            .into_iter()
            .map(|t| json!({"type": "input_text", "text": t}))
            .collect();
        input.push(json!({
            "type": "message",
            "role": "user",
            "content": content,
        }));
    }

    for (j, (name, resp)) in func_responses.into_iter().enumerate() {
        let call_id = pending_call_ids
            .get(j)
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("call_gf_orphan_{}_{}", name, j));
        let output_str = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into());
        input.push(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output_str,
        }));
    }
}

/// Flattens a Gemini model-role parts array into Responses input items.
/// Returns the synthesized call_ids in declaration order so the next user
/// turn can align its `functionResponse` parts.
fn append_model_content(
    input: &mut Vec<Value>,
    parts: &[Value],
    call_id_counter: &mut u32,
) -> Vec<String> {
    let mut new_pending: Vec<String> = Vec::new();
    for p in parts {
        let Some(part) = p.as_object() else { continue };
        if let Some(fc) = part.get("functionCall").and_then(|v| v.as_object()) {
            let name = fc
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = fc
                .get("args")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            let args_json = serde_json::to_string(&args).unwrap_or_else(|_| "{}".into());
            let call_id = format!("call_gf_{}", *call_id_counter);
            *call_id_counter += 1;
            new_pending.push(call_id.clone());
            input.push(json!({
                "type": "function_call",
                "name": name,
                "call_id": call_id,
                "arguments": args_json,
            }));
            continue;
        }
        if let Some(txt) = part.get("text").and_then(|v| v.as_str()) {
            if !txt.is_empty() {
                input.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": txt}],
                }));
            }
        }
        // thoughtSignature, inlineData, fileData and other Gemini-specific
        // parts are intentionally dropped — augment cannot consume them.
    }
    new_pending
}

/// Walks a JSON-Schema-shaped value and lowercases every string value of a
/// key named `type`. Other fields and nested arrays are recursed without
/// modification.
fn normalize_schema_type_case(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, val) in map {
                if k == "type" {
                    if let Value::String(s) = &val {
                        out.insert(k, Value::String(s.to_ascii_lowercase()));
                        continue;
                    }
                }
                out.insert(k, normalize_schema_type_case(val));
            }
            Value::Object(out)
        }
        Value::Array(arr) => {
            Value::Array(arr.into_iter().map(normalize_schema_type_case).collect())
        }
        other => other,
    }
}

/// Converts a non-streaming OpenAI Responses JSON body into a Gemini
/// `generateContent` JSON body. Reasoning items are dropped (Gemini has no
/// equivalent); message items become text parts; function_call items become
/// Gemini `functionCall` parts.
pub fn translate_gemini_response(resp_body: &[u8], original_model: &str) -> Result<Vec<u8>> {
    if resp_body.is_empty() {
        return build_gemini_response(&[], 0, 0, 0, "", original_model);
    }
    let v: Value =
        serde_json::from_slice(resp_body).context("gemini translate: parse responses body")?;
    let empty: Vec<Value> = Vec::new();
    let items = v.get("output").and_then(|x| x.as_array()).unwrap_or(&empty);
    let usage_in = v
        .pointer("/usage/input_tokens")
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    let usage_out = v
        .pointer("/usage/output_tokens")
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    let usage_total = v
        .pointer("/usage/total_tokens")
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    let upstream_model = v.get("model").and_then(|x| x.as_str()).unwrap_or("");
    build_gemini_response(
        items,
        usage_in,
        usage_out,
        usage_total,
        upstream_model,
        original_model,
    )
}

/// Builds the Gemini envelope from an accumulated list of OpenAI output
/// items plus usage numbers.
fn build_gemini_response(
    items: &[Value],
    usage_in: i64,
    usage_out: i64,
    usage_total: i64,
    upstream_model: &str,
    requested_model: &str,
) -> Result<Vec<u8>> {
    let mut parts: Vec<Value> = Vec::with_capacity(items.len());
    for raw in items {
        let item_type = raw.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match item_type {
            "reasoning" => {} // not representable in Gemini shape
            "message" => {
                let text = collect_message_output_text(raw);
                if !text.is_empty() {
                    parts.push(json!({"text": text}));
                }
            }
            "function_call" => {
                let name = raw
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let raw_args = raw.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                let args: Value = if raw_args.is_empty() {
                    Value::Object(Map::new())
                } else {
                    serde_json::from_str(raw_args).unwrap_or_else(|_| Value::Object(Map::new()))
                };
                parts.push(json!({"functionCall": {"name": name, "args": args}}));
            }
            _ => {}
        }
    }

    if parts.is_empty() {
        parts.push(json!({"text": ""}));
    }

    let model_version = if requested_model.is_empty() {
        upstream_model.to_string()
    } else {
        requested_model.to_string()
    };
    let total = if usage_total == 0 {
        usage_in + usage_out
    } else {
        usage_total
    };

    let now = chrono::Utc::now();
    let resp = json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": parts,
            },
            "finishReason": "STOP",
            "index": 0,
        }],
        "usageMetadata": {
            "promptTokenCount": usage_in,
            "candidatesTokenCount": usage_out,
            "totalTokenCount": total,
        },
        "modelVersion": model_version,
        "createTime": now.to_rfc3339(),
        "responseId": format!("amp-proxy-{}", now.timestamp_nanos_opt().unwrap_or(0)),
    });
    serde_json::to_vec(&resp).context("gemini translate: marshal response")
}

/// Extracts the concatenated text from an OpenAI message output item,
/// tolerating both string-valued and array-of-parts content shapes.
fn collect_message_output_text(raw: &Value) -> String {
    let Some(content) = raw.get("content") else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    let mut buf = String::new();
    for part in parts {
        let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if part_type != "output_text" && part_type != "text" {
            continue;
        }
        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
            buf.push_str(t);
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEMINI_SINGLE_TURN: &str = r#"{
      "contents":[
        {"role":"user","parts":[{"text":"Find tailwind configs in my-journal-app"}]}
      ],
      "systemInstruction":{
        "role":"user",
        "parts":[{"text":"You are a fast, parallel code search agent."}]
      },
      "tools":[{"functionDeclarations":[
        {
          "name":"glob",
          "description":"Fast file pattern matching tool",
          "parameters":{
            "type":"OBJECT",
            "required":["filePattern"],
            "properties":{
              "filePattern":{"type":"STRING","description":"Glob pattern"},
              "limit":{"type":"NUMBER","description":"Max results"}
            }
          }
        }
      ]}],
      "generationConfig":{"temperature":1,"maxOutputTokens":65535}
    }"#;

    #[test]
    fn bare_text_request_translates() {
        let bare = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
        let out = translate_gemini_request_to_openai(bare.as_bytes(), "gpt-5.4-mini").unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "gpt-5.4-mini");
        assert_eq!(v["stream"], true);
        assert!(v.get("reasoning").is_none(), "no reasoning without suffix");
        let input = v["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "hi");
    }

    #[test]
    fn request_with_tools_and_thinking_suffix() {
        let out =
            translate_gemini_request_to_openai(GEMINI_SINGLE_TURN.as_bytes(), "gpt-5.4-mini(high)")
                .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "gpt-5.4-mini", "suffix must be stripped");
        assert_eq!(v["reasoning"]["effort"], "high");
        assert_eq!(v["max_output_tokens"], 65535);
        assert_eq!(v["include"][0], "reasoning.encrypted_content");

        let input = v["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "system");
        assert!(input[0]["content"]
            .as_str()
            .unwrap()
            .contains("parallel code search agent"));

        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "glob");
        assert_eq!(
            tools[0]["parameters"]["type"], "object",
            "OBJECT must lowercase to object"
        );
        assert_eq!(
            tools[0]["parameters"]["properties"]["filePattern"]["type"],
            "string"
        );
        assert_eq!(
            tools[0]["parameters"]["properties"]["limit"]["type"],
            "number"
        );
    }

    #[test]
    fn request_with_function_call_pairs_call_ids() {
        let req = r#"{
          "contents":[
            {"role":"user","parts":[{"text":"go"}]},
            {"role":"model","parts":[
              {"functionCall":{"name":"glob","args":{"filePattern":"**/x"}},"thoughtSignature":"OPAQUE=="},
              {"functionCall":{"name":"Grep","args":{"pattern":"y"}}}
            ]},
            {"role":"user","parts":[
              {"functionResponse":{"name":"glob","response":{"out":["a"]}}},
              {"functionResponse":{"name":"Grep","response":{"out":["b"]}}}
            ]}
          ]
        }"#;
        let out = translate_gemini_request_to_openai(req.as_bytes(), "gpt-5.4-mini").unwrap();
        // thoughtSignature must be dropped entirely.
        let bytes_str = std::str::from_utf8(&out).unwrap();
        assert!(!bytes_str.contains("thoughtSignature"));
        assert!(!bytes_str.contains("OPAQUE=="));

        let v: Value = serde_json::from_slice(&out).unwrap();
        let input = v["input"].as_array().unwrap();
        // user-msg, fc(0), fc(1), fco(0), fco(1)
        assert_eq!(input.len(), 5);
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_gf_0");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_gf_1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_gf_0");
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["call_id"], "call_gf_1");
        // output is a JSON string, not nested obj.
        let out_str = input[3]["output"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(out_str).unwrap();
        assert_eq!(parsed["out"][0], "a");
    }

    #[test]
    fn response_with_function_call_translates_back() {
        let openai = r#"{
          "id":"resp_x","status":"completed","model":"gpt-5.4-mini",
          "output":[
            {"type":"reasoning","id":"rs_1","summary":[]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]},
            {"type":"function_call","name":"glob","call_id":"call_1","arguments":"{\"filePattern\":\"*.md\"}"}
          ],
          "usage":{"input_tokens":100,"output_tokens":5,"total_tokens":105}
        }"#;
        let out = translate_gemini_response(openai.as_bytes(), "gemini-3-flash-preview").unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let parts = v["candidates"][0]["content"]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2, "reasoning dropped, msg + fc kept");
        assert_eq!(parts[0]["text"], "ok");
        assert_eq!(parts[1]["functionCall"]["name"], "glob");
        assert_eq!(parts[1]["functionCall"]["args"]["filePattern"], "*.md");
        // Gemini has no call_id field; must NOT be present.
        assert!(parts[1]["functionCall"].get("call_id").is_none());
        assert_eq!(v["modelVersion"], "gemini-3-flash-preview");
        assert_eq!(v["usageMetadata"]["promptTokenCount"], 100);
        assert_eq!(v["usageMetadata"]["candidatesTokenCount"], 5);
        assert_eq!(v["usageMetadata"]["totalTokenCount"], 105);
        assert_eq!(v["candidates"][0]["finishReason"], "STOP");
    }

    #[test]
    fn normalize_schema_preserves_enum_case() {
        let input = json!({
            "type": "OBJECT",
            "properties": {
                "x": {
                    "type": "ARRAY",
                    "description": "kept as-is",
                    "items": {"type": "STRING", "enum": ["FOO", "BAR"]}
                },
                "y": {"type": "INTEGER"}
            }
        });
        let got = normalize_schema_type_case(input);
        assert_eq!(got["type"], "object");
        assert_eq!(got["properties"]["x"]["type"], "array");
        assert_eq!(got["properties"]["x"]["description"], "kept as-is");
        assert_eq!(got["properties"]["x"]["items"]["type"], "string");
        assert_eq!(got["properties"]["x"]["items"]["enum"][0], "FOO");
        assert_eq!(got["properties"]["y"]["type"], "integer");
    }
}
