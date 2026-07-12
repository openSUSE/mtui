//! Process-level command-line arguments for the `mtui-mcp` server.
//!
//! Port of upstream `mtui/mcp/args.py`. Deliberately a **subset** of the REPL's
//! [`mtui_core::Args`]: it declares only the flags `Config` merging + logging
//! care about (`-c/--config`, `-t/--template-dir`, `-w/--connection-timeout`,
//! `-g/--gitea-token`, `--color`, `-d/--debug`; clap auto-handles `-V/--version`
//! and `--help`). The REPL-only update/SUT flags (`-a`/`-k`/`--sut`) are omitted:
//! the MCP server loads templates and hosts *per session* at runtime via the
//! `load_template` / `add_host` tools, so it takes no boot-time update/SUT flags.
//!
//! Three MCP-server flags are added: `--transport`, `--host`, `--port`. Only
//! `stdio` is served in this bead; `--transport http` (per-client session
//! isolation) is bead `mtui-rs-76e.10` and is currently rejected with a clear
//! not-yet-implemented error (see [`crate::main`]).

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use mtui_config::{Config, SslVerify};
use mtui_core::ColorArg;

/// Top-level `mtui-mcp` process arguments.
///
/// Parse with [`McpArgs::parse`] (exits on `--help`/`--version`/error) or
/// [`McpArgs::try_parse_from`] (returns a `clap::Error`, for tests).
#[derive(Debug, Parser)]
#[command(
    name = "mtui-mcp",
    // Reuse the REPL's build-provenance block for `-V`/`--version`.
    version = env!("MTUI_LONG_VERSION"),
    long_version = env!("MTUI_LONG_VERSION"),
    about = "MCP server for mtui (tools synthesised from the command registry)",
    long_about = None,
    disable_help_subcommand = true
)]
pub struct McpArgs {
    /// Override config `mtui.template_dir`.
    #[arg(short = 't', long = "template-dir", value_name = "DIR")]
    pub template_dir: Option<PathBuf>,

    /// Override config `mtui.connection_timeout` (seconds).
    #[arg(short = 'w', long = "connection-timeout", value_name = "SECONDS")]
    pub connection_timeout: Option<u64>,

    /// Enable debugging output.
    #[arg(short = 'd', long = "debug")]
    pub debug: bool,

    /// Override the default config path.
    #[arg(short = 'c', long = "config", value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Control coloured (log) output. Logs go to stderr; stdout is the transport.
    #[arg(long = "color", value_enum, default_value_t = ColorArg::Auto)]
    pub color: ColorArg,

    /// Gitea access token.
    #[arg(short = 'g', long = "gitea-token", value_name = "TOKEN")]
    pub gitea_token: Option<String>,

    /// Override config `mtui.ssl_verify`: TLS certificate verification for all
    /// outbound HTTP. Accepts `true`/`false` (and the spellings `yes`/`no`/
    /// `on`/`off`/`1`/`0`), or a path to a custom CA bundle/certificate.
    #[arg(long = "ssl-verify", value_name = "BOOL|PATH")]
    pub ssl_verify: Option<String>,

    /// MCP transport to serve on. Only `stdio` is implemented; `http` (per-client
    /// session isolation) is bead mtui-rs-76e.10.
    #[arg(long = "transport", value_enum, default_value_t = Transport::Stdio)]
    pub transport: Transport,

    /// Bind address for `--transport http` (default: 127.0.0.1). Ignored under
    /// stdio.
    #[arg(long = "host", value_name = "ADDR", default_value = "127.0.0.1")]
    pub host: String,

    /// Bind port for `--transport http` (default: 8000). Ignored under stdio.
    #[arg(long = "port", value_name = "PORT", default_value_t = 8000)]
    pub port: u16,
}

impl McpArgs {
    /// Load the config from the file chain (keyed on `--config`) and overlay the
    /// CLI overrides, returning the fully-resolved [`Config`].
    ///
    /// Mirrors upstream `Config(args.config)` + `Config.merge_args(args)`: the
    /// file layers first ([`Config::load`]), then the CLI layer on top so a flag
    /// wins over every config file. `--config`, `--debug`, `--color`, and the
    /// transport flags are not config keys and are intentionally not merged.
    #[must_use]
    pub fn resolve_config(&self) -> Config {
        let mut config = Config::load(self.config.clone());
        if let Some(dir) = &self.template_dir {
            config.template_dir = dir.clone();
        }
        if let Some(timeout) = self.connection_timeout {
            config.connection_timeout = timeout;
        }
        if let Some(token) = &self.gitea_token {
            config.gitea_token = token.clone();
        }
        if let Some(raw) = &self.ssl_verify {
            config.ssl_verify = SslVerify::parse(raw);
        }
        config
    }
}

/// The `--transport` choice.
///
/// `stdio` (one process == one client) is the only transport served in this
/// bead; `http` is reserved for the per-client session registry (mtui-rs-76e.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Transport {
    /// Serve over stdin/stdout (default). One process serves one client.
    #[default]
    Stdio,
    /// Serve over streamable HTTP (not yet implemented — mtui-rs-76e.10).
    Http,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<McpArgs, clap::Error> {
        let mut full = vec!["mtui-mcp"];
        full.extend_from_slice(argv);
        McpArgs::try_parse_from(full)
    }

    #[test]
    fn no_args_leaves_defaults() {
        let a = parse(&[]).unwrap();
        assert!(a.template_dir.is_none());
        assert!(a.connection_timeout.is_none());
        assert!(!a.debug);
        assert!(a.config.is_none());
        assert_eq!(a.color, ColorArg::Auto);
        assert!(a.gitea_token.is_none());
        assert!(a.ssl_verify.is_none());
        assert_eq!(a.transport, Transport::Stdio);
        assert_eq!(a.host, "127.0.0.1");
        assert_eq!(a.port, 8000);
    }

    #[test]
    fn scalar_flags_parse() {
        let a = parse(&[
            "-t",
            "/tmp/tpl",
            "-w",
            "42",
            "-c",
            "/etc/mtui.toml",
            "-g",
            "tok",
            "-d",
            "--color",
            "never",
        ])
        .unwrap();
        assert_eq!(a.template_dir.unwrap().to_str().unwrap(), "/tmp/tpl");
        assert_eq!(a.connection_timeout.unwrap(), 42);
        assert_eq!(a.config.unwrap().to_str().unwrap(), "/etc/mtui.toml");
        assert_eq!(a.gitea_token.unwrap(), "tok");
        assert!(a.debug);
        assert_eq!(a.color, ColorArg::Never);
    }

    #[test]
    fn transport_choices_parse_and_reject() {
        assert_eq!(
            parse(&["--transport", "http"]).unwrap().transport,
            Transport::Http
        );
        assert_eq!(
            parse(&["--transport", "stdio"]).unwrap().transport,
            Transport::Stdio
        );
        assert!(parse(&["--transport", "carrier-pigeon"]).is_err());
    }

    #[test]
    fn repl_only_flags_are_rejected() {
        // -a/-k/--sut are REPL-only; the MCP parser must not accept them.
        assert!(parse(&["-a", "SUSE:Maintenance:1:1"]).is_err());
        assert!(parse(&["--sut", "host1"]).is_err());
    }

    #[test]
    fn resolve_config_applies_cli_over_file_defaults() {
        let cfg = parse(&["--ssl-verify", "false"]).unwrap().resolve_config();
        assert_eq!(cfg.ssl_verify, SslVerify::Disabled);
    }

    #[test]
    fn resolve_config_overlays_scalars() {
        let cfg = parse(&["-w", "77", "-g", "abc", "-t", "/tmp/tpl"])
            .unwrap()
            .resolve_config();
        assert_eq!(cfg.connection_timeout, 77);
        assert_eq!(cfg.gitea_token, "abc");
        assert_eq!(cfg.template_dir, PathBuf::from("/tmp/tpl"));
    }
}
