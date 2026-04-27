//! Pretty-printer for `body_capture` debug log files.

use std::path::Path;

use anyhow::{bail, Context};
use serde_json::{Map, Value};

const REQUEST_MARKER: &str = "=== REQUEST ===\n";
const RESPONSE_SEPARATOR: &str = "\n\n=== RESPONSE ===\n";
const REQUEST_MARKER_CRLF: &str = "=== REQUEST ===\r\n";
const RESPONSE_SEPARATOR_CRLF: &str = "\r\n\r\n=== RESPONSE ===\r\n";

/// Parse a `body_capture` `.log` file into structured JSON.
pub fn parse_capture_log(input: &str) -> anyhow::Result<Value> {
    let (after_request, response_separator, line_ending) =
        if let Some(rest) = input.strip_prefix(REQUEST_MARKER) {
            (rest, RESPONSE_SEPARATOR, "\n")
        } else if let Some(rest) = input.strip_prefix(REQUEST_MARKER_CRLF) {
            (rest, RESPONSE_SEPARATOR_CRLF, "\r\n")
        } else {
            bail!("capture log must start with === REQUEST ===");
        };

    let (request_section, response_section) = after_request
        .split_once(response_separator)
        .context("capture log missing response separator")?;

    let request = parse_request(request_section, line_ending)?;
    let response = parse_response(response_section, line_ending)?;

    Ok(serde_json::json!({
        "request": request,
        "response": response,
    }))
}

/// Format a `body_capture` `.log` string as pretty JSON.
pub fn format_capture_log(input: &str) -> anyhow::Result<String> {
    let value = parse_capture_log(input)?;
    serde_json::to_string_pretty(&value).context("format pretty JSON")
}

/// Read a `body_capture` `.log` file and format it as pretty JSON.
pub fn format_capture_file(path: &Path) -> anyhow::Result<String> {
    let input = std::fs::read_to_string(path)
        .with_context(|| format!("read capture log {}", path.display()))?;
    format_capture_log(&input)
}

/// Convert a `body_capture` `.log` file and write the pretty JSON to `output_path`.
pub fn write_pretty_file(input_path: &Path, output_path: &Path) -> anyhow::Result<()> {
    let pretty = format_capture_file(input_path)?;
    std::fs::write(output_path, pretty)
        .with_context(|| format!("write pretty capture JSON {}", output_path.display()))
}

fn parse_request(section: &str, line_ending: &str) -> anyhow::Result<Value> {
    let (request_line, rest) = split_once_line(section, line_ending)
        .context("request section missing METHOD path line")?;
    let (method, path) = request_line
        .split_once(' ')
        .context("request line must be METHOD path")?;
    if method.is_empty() || path.is_empty() {
        bail!("request line must be METHOD path");
    }

    let (header_lines, body) = split_headers_and_body(rest, line_ending)
        .context("request section missing blank line after headers")?;

    Ok(serde_json::json!({
        "method": method,
        "path": path,
        "headers": parse_headers(header_lines)?,
        "body": parse_body(body),
    }))
}

fn parse_response(section: &str, line_ending: &str) -> anyhow::Result<Value> {
    let (status_line, rest) =
        split_once_line(section, line_ending).context("response section missing status line")?;
    let status = status_line
        .strip_prefix("status: ")
        .context("response status line must be status: N")?
        .parse::<u16>()
        .context("parse response status")?;

    let (header_lines, body) = split_headers_and_body(rest, line_ending)
        .context("response section missing blank line after headers")?;
    let body = strip_capture_terminal_newline(body);

    Ok(serde_json::json!({
        "status": status,
        "headers": parse_headers(header_lines)?,
        "body": parse_body(body),
    }))
}

fn split_once_line<'a>(input: &'a str, line_ending: &str) -> Option<(&'a str, &'a str)> {
    input.split_once(line_ending)
}

fn split_headers_and_body<'a>(input: &'a str, line_ending: &str) -> Option<(&'a str, &'a str)> {
    if let Some(body) = input.strip_prefix(line_ending) {
        return Some(("", body));
    }

    let separator = format!("{line_ending}{line_ending}");
    input.split_once(&separator)
}

fn parse_headers(lines: &str) -> anyhow::Result<Value> {
    let mut headers = Map::new();
    for line in lines.lines().filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .with_context(|| format!("invalid header line: {line}"))?;
        let value = value.strip_prefix(' ').unwrap_or(value).to_string();
        insert_header(&mut headers, name.to_string(), Value::String(value));
    }
    Ok(Value::Object(headers))
}

fn insert_header(headers: &mut Map<String, Value>, name: String, value: Value) {
    match headers.get_mut(&name) {
        Some(Value::Array(values)) => values.push(value),
        Some(existing) => {
            let first = std::mem::replace(existing, Value::Null);
            *existing = Value::Array(vec![first, value]);
        }
        None => {
            headers.insert(name, value);
        }
    }
}

fn parse_body(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_string()))
}

fn strip_capture_terminal_newline(body: &str) -> &str {
    body.strip_suffix("\r\n")
        .or_else(|| body.strip_suffix('\n'))
        .unwrap_or(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    const JSON_CAPTURE: &str = "=== REQUEST ===\nPOST /v1/responses\ncontent-type: application/json\nauthorization: [REDACTED]\n\n{\"model\":\"gpt\",\"input\":[1,2]}\n\n=== RESPONSE ===\nstatus: 200\ncontent-type: application/json\n\n{\"ok\":true,\"token\":\"[REDACTED]\"}\n";

    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("amp-proxy-capture-pretty-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_json_body_as_json() {
        let pretty = parse_capture_log(JSON_CAPTURE).unwrap();

        assert_eq!(pretty["request"]["method"], "POST");
        assert_eq!(pretty["request"]["path"], "/v1/responses");
        assert_eq!(pretty["request"]["headers"]["authorization"], "[REDACTED]");
        assert_eq!(pretty["request"]["body"]["model"], "gpt");
        assert_eq!(pretty["request"]["body"]["input"][1], 2);
        assert_eq!(pretty["response"]["status"], 200);
        assert_eq!(pretty["response"]["body"]["ok"], true);
        assert_eq!(pretty["response"]["body"]["token"], "[REDACTED]");
    }

    #[test]
    fn parses_non_json_body_as_string() {
        let input = "=== REQUEST ===\nGET /plain\naccept: text/plain\n\nhello request\n\n=== RESPONSE ===\nstatus: 502\ncontent-type: text/plain\n\nupstream unavailable\n";
        let pretty = parse_capture_log(input).unwrap();

        assert_eq!(pretty["request"]["body"], "hello request");
        assert_eq!(pretty["response"]["body"], "upstream unavailable");
    }

    #[test]
    fn writes_output_file() {
        let dir = unique_temp_dir("write-output");
        let input_path = dir.join("capture.log");
        let output_path = dir.join("capture.pretty.json");
        std::fs::write(&input_path, JSON_CAPTURE).unwrap();

        write_pretty_file(&input_path, &output_path).unwrap();

        let output = std::fs::read_to_string(&output_path).unwrap();
        let parsed: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["request"]["method"], "POST");
        assert_eq!(parsed["response"]["body"]["ok"], true);

        let _ = std::fs::remove_dir_all(dir);
    }
}
