//! `amp-proxy init` interactive wizard.
//!
//! Ported from `cmd/amp-proxy/init.go`. Prompts the operator for the handful
//! of values that cannot be defaulted, then writes a ready-to-run
//! `config.yaml`. Hard-coded defaults match the Go version so generated files
//! are byte-for-byte equivalent (modulo YAML quoting differences).
//!
//! Anything that would need real customisation (multiple providers, per-client
//! upstream keys) is intentionally out of scope; operators who need that can
//! hand-edit afterwards.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use clap::Parser;

use crate::amp::secret::generate_api_key;

/// CLI args for the `amp-proxy init` subcommand.
#[derive(Debug, Parser)]
#[command(name = "init", about = "Generate a ready-to-run amp-proxy config.yaml")]
pub struct InitArgs {
    /// Path to write the generated config file.
    #[arg(long, default_value = "config.yaml")]
    pub config: PathBuf,
    /// Overwrite the target file if it already exists.
    #[arg(long)]
    pub force: bool,
}

/// Entry point for the `amp-proxy init` subcommand. Reads from stdin /
/// writes to stdout for prompts; on success the generated config lands at
/// `args.config` with mode 600 (POSIX) or default permissions (Windows).
pub fn run(args: InitArgs) -> anyhow::Result<()> {
    if args.config.exists() && !args.force {
        anyhow::bail!(
            "refusing to overwrite existing {} — delete it, pass --force, or use --config <other-path>",
            args.config.display()
        );
    }

    println!("amp-proxy init — answer a few questions and a ready-to-run config will be written.");
    println!("Values are echoed to the terminal; clear your shell history if the API key is sensitive.");
    println!();

    let stdin = io::stdin();
    let mut reader = stdin.lock();

    let gateway_url = prompt_required(
        &mut reader,
        "Custom provider URL (OpenAI-compatible, e.g. http://host:port/v1)",
        "",
    )?;
    let gateway_key = prompt_required(&mut reader, "Custom provider API key (Bearer token)", "")?;
    let gemini_mode = prompt_choice(
        &mut reader,
        "Gemini route mode",
        &["translate", "ampcode"],
        "translate",
    )?;
    let amp_upstream = prompt_optional(
        &mut reader,
        "Amp upstream API key (for ampcode.com fallback, press Enter to skip)",
        "",
    )?;

    let local_key = generate_api_key();
    let content = render_init_config(
        &gateway_url,
        &gateway_key,
        &amp_upstream,
        &gemini_mode,
        &local_key,
    );

    write_config_file(&args.config, &content)?;

    println!();
    println!("Wrote {} (mode 600 on POSIX).", args.config.display());
    println!();
    println!("Start amp-proxy:");
    println!("  ./amp-proxy --config {}", args.config.display());
    println!();
    println!("Point Amp CLI at it:");
    println!("  export AMP_URL=http://127.0.0.1:8317");
    println!("  export AMP_API_KEY={local_key}");
    println!("  amp");
    Ok(())
}

fn write_config_file(path: &PathBuf, content: &str) -> anyhow::Result<()> {
    fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn prompt_required<R: BufRead>(
    r: &mut R,
    label: &str,
    default_val: &str,
) -> anyhow::Result<String> {
    loop {
        if !default_val.is_empty() {
            print!("{label} [{default_val}]: ");
        } else {
            print!("{label}: ");
        }
        io::stdout().flush().ok();
        let mut line = String::new();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            // EOF.
            if !default_val.is_empty() {
                return Ok(default_val.to_string());
            }
            anyhow::bail!("read {label}: EOF");
        }
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            if !default_val.is_empty() {
                return Ok(default_val.to_string());
            }
            println!("  value required, please try again");
            continue;
        }
        return Ok(trimmed);
    }
}

fn prompt_optional<R: BufRead>(
    r: &mut R,
    label: &str,
    default_val: &str,
) -> anyhow::Result<String> {
    if !default_val.is_empty() {
        print!("{label} [{default_val}]: ");
    } else {
        print!("{label}: ");
    }
    io::stdout().flush().ok();
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Ok(default_val.to_string());
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(default_val.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn prompt_choice<R: BufRead>(
    r: &mut R,
    label: &str,
    choices: &[&str],
    default_val: &str,
) -> anyhow::Result<String> {
    let lowered: Vec<String> = choices.iter().map(|c| c.to_lowercase()).collect();
    loop {
        print!("{label} ({}) [{default_val}]: ", choices.join("/"));
        io::stdout().flush().ok();
        let mut line = String::new();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Ok(default_val.to_string());
        }
        let trimmed = line.trim().to_lowercase();
        if trimmed.is_empty() {
            return Ok(default_val.to_string());
        }
        if lowered.iter().any(|c| *c == trimmed) {
            return Ok(trimmed);
        }
        println!("  invalid choice {trimmed:?}, must be one of {:?}", choices);
    }
}

/// 9-entry mapping table from the Go init.go. Pinned here so generated configs
/// match upstream byte-for-byte.
const DEFAULT_MAPPINGS: &[(&str, &str)] = &[
    ("claude-opus-4-6", "gpt-5.4(high)"),
    ("claude-sonnet-4-6-thinking", "gpt-5.4-mini(high)"),
    ("claude-haiku-4-5-20251001", "gpt-5.4-mini"),
    ("gpt-5.4", "gpt-5.4(xhigh)"),
    ("gemini-2.5-flash-lite-preview-09-2025", "gpt-5.4-mini"),
    ("gemini-2.5-flash-lite", "gpt-5.4-mini"),
    ("claude-sonnet-4-6", "gpt-5.4-mini(high)"),
    ("gpt-5.3-codex", "gpt-5.4(high)"),
    ("gemini-3-flash-preview", "gpt-5.4-mini(high)"),
];

/// Render the generated config.yaml body. Layout intentionally mirrors the Go
/// version so operators who switch between binaries see the same comments.
pub fn render_init_config(
    gateway_url: &str,
    gateway_key: &str,
    amp_upstream: &str,
    gemini_mode: &str,
    local_key: &str,
) -> String {
    let mut b = String::new();
    b.push_str("# Generated by `amp-proxy init`.\n");
    b.push_str("# Edit freely — amp-proxy hot-reloads most fields without restart.\n");
    b.push('\n');
    b.push_str("host: \"127.0.0.1\"\n");
    b.push_str("port: 8317\n");
    b.push('\n');
    b.push_str("# Local API keys Amp CLI must present (match AMP_API_KEY in your shell).\n");
    b.push_str("api-keys:\n");
    b.push_str(&format!("  - {}\n", yaml_string(local_key)));
    b.push('\n');
    b.push_str("ampcode:\n");
    b.push_str("  upstream-url: \"https://ampcode.com\"\n");
    b.push_str(&format!(
        "  upstream-api-key: {}\n",
        yaml_string(amp_upstream)
    ));
    b.push_str("  restrict-management-to-localhost: true\n");
    b.push('\n');
    b.push_str("  # Rewrite Amp CLI model names onto the gpt-5.4 family served by\n");
    b.push_str("  # custom-providers below. Adjust the right-hand side if your gateway\n");
    b.push_str("  # exposes different model names.\n");
    b.push_str("  model-mappings:\n");
    for (from, to) in DEFAULT_MAPPINGS {
        b.push_str(&format!("    - from: {}\n", yaml_string(from)));
        b.push_str(&format!("      to: {}\n", yaml_string(to)));
    }
    b.push('\n');
    b.push_str("  force-model-mappings: true\n");
    b.push('\n');
    b.push_str("  custom-providers:\n");
    b.push_str("    - name: \"gateway\"\n");
    b.push_str(&format!("      url: {}\n", yaml_string(gateway_url)));
    b.push_str(&format!("      api-key: {}\n", yaml_string(gateway_key)));
    b.push_str("      models:\n");
    b.push_str("        - \"gpt-5.4\"\n");
    b.push_str("        - \"gpt-5.4-mini\"\n");
    b.push('\n');
    b.push_str(&format!(
        "  gemini-route-mode: {}\n",
        yaml_string(gemini_mode)
    ));
    b
}

/// Quote a string for safe embedding in YAML. We use double-quotes and
/// escape backslashes / inner quotes to mirror Go's `%q` verb.
fn yaml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn render_then_parse_round_trips() {
        let yaml = render_init_config(
            "http://example.com:8000/v1",
            "sk-abc123",
            "amp-upstream-key",
            "translate",
            "amp-localtoken",
        );
        let cfg: Config = serde_yaml::from_str(&yaml)
            .expect("generated config must parse via the production loader");
        assert_eq!(cfg.port, 8317);
        assert_eq!(cfg.api_keys, vec!["amp-localtoken".to_string()]);
        assert_eq!(cfg.ampcode.upstream_url, "https://ampcode.com");
        assert_eq!(cfg.ampcode.upstream_api_key, "amp-upstream-key");
        assert_eq!(cfg.ampcode.gemini_route_mode, "translate");
        assert!(cfg.ampcode.force_model_mappings);
        assert!(cfg.ampcode.restrict_management_to_localhost);
        assert_eq!(cfg.ampcode.model_mappings.len(), DEFAULT_MAPPINGS.len());
        assert_eq!(cfg.ampcode.custom_providers.len(), 1);
        let p = &cfg.ampcode.custom_providers[0];
        assert_eq!(p.name, "gateway");
        assert_eq!(p.url, "http://example.com:8000/v1");
        assert_eq!(p.api_key, "sk-abc123");
        assert_eq!(p.models, vec!["gpt-5.4", "gpt-5.4-mini"]);
    }

    #[test]
    fn render_then_validate_passes() {
        let yaml = render_init_config(
            "http://example.com:8000/v1",
            "sk-abc123",
            "amp-up",
            "translate",
            "amp-local",
        );
        let cfg: Config = serde_yaml::from_str(&yaml).unwrap();
        cfg.validate().expect("generated config must validate");
    }

    #[test]
    fn yaml_string_escapes_quotes() {
        assert_eq!(yaml_string(r#"a "b" c"#), r#""a \"b\" c""#);
        assert_eq!(yaml_string("x\\y"), "\"x\\\\y\"");
        assert_eq!(yaml_string(""), "\"\"");
    }

    #[test]
    fn run_writes_file_and_refuses_overwrite() {
        // Smoke test that doesn't drive the interactive prompts. We use the
        // render path directly and write through the same helper run() does.
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "amp-proxy-init-test-{}.yaml",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_file(&path).ok();
        }
        let yaml = render_init_config(
            "http://example.com:8000/v1",
            "k",
            "",
            "translate",
            "amp-local",
        );
        write_config_file(&path, &yaml).unwrap();
        assert!(path.exists());

        // Re-running with force=false should refuse via the CLI struct.
        let args = InitArgs {
            config: path.clone(),
            force: false,
        };
        let err = run(args).err().expect("must refuse to overwrite");
        assert!(err.to_string().contains("refusing to overwrite"));

        fs::remove_file(&path).ok();
    }
}
