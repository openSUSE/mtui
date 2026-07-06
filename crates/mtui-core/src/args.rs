//! Process-level command-line arguments (`Args`).
//!
//! Port of upstream `mtui.cli.args.get_parser` — the *top-level* argument parser
//! for the `mtui` process, distinct from the per-command `clap::Command`s the
//! [`engine`](crate::engine) builds from the registry. It declares the global
//! flags that configure the whole session (config/template overrides, SUT host
//! overrides, colour mode, Gitea token) and the mutually-exclusive update
//! selector that seeds a workflow.
//!
//! ## Intentional deviations from upstream
//!
//! This is a redesign, not a 1:1 transpile (see `AGENTS.md`), so a few surfaces
//! differ where it improves the tool:
//!
//! * **`-V/--version`** prints `mtui <version> (<sha>[-dirty], <profile>,
//!   <target>)`. Upstream listed separately-installed *runtime* dependency
//!   versions (paramiko, openqa-client) because those could drift per operator
//!   environment; a statically-compiled binary has no such drift (deps are
//!   compiled in at lockfile-pinned versions), so that block would be redundant.
//!   What *does* vary for an out-of-tree build is the build provenance — commit,
//!   profile, target — which [`build.rs`](../../build.rs) captures into the
//!   `MTUI_LONG_VERSION` env var fed to clap's `long_version` below. Outside a
//!   git checkout the sha field is omitted; profile and target are always shown.
//! * **`-a/--auto-review-id` vs `-k/--kernel-review-id`** upstream construct two
//!   distinct `UpdateID` subclasses that later stamp `TestReport.workflow`. Here
//!   both parse into the same [`UpdateID`] value type (all this crate has today)
//!   paired with the [`Workflow`] the flag selects, surfaced as [`Args::update`].
//!
//! Config merging (`Config::merge_args`) and the `auto` → TTY/`NO_COLOR`
//! resolution of [`ColorArg`] into [`ColorMode`](crate::display::ColorMode) are
//! owned by their consuming tasks (the non-interactive dispatch entrypoint and
//! the REPL binary), not by this parser.

use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, ValueEnum};
use mtui_types::{UpdateID, Workflow};

/// Top-level `mtui` process arguments.
///
/// Parse with [`Args::parse`] (exits the process on `--help`/`--version`/error,
/// for the real binary) or [`Args::try_parse_from`] (returns a `clap::Error`,
/// for tests and embedding). The mutually-exclusive `-a`/`-k` pair is resolved
/// through [`Args::update`].
#[derive(Debug, Parser)]
#[command(
    name = "mtui",
    // Both `-V` and `--version` carry the full provenance block: clap uses
    // `version` for `-V` and `long_version` for `--version`, so set both.
    version = env!("MTUI_LONG_VERSION"),
    long_version = env!("MTUI_LONG_VERSION"),
    about = "Maintenance Test Update Installer",
    long_about = None,
    disable_help_subcommand = true
)]
pub struct Args {
    /// Override config `mtui.template_dir`.
    #[arg(short = 't', long = "template-dir", value_name = "DIR")]
    pub template_dir: Option<PathBuf>,

    /// Cumulatively override the default hosts from the template
    /// (format: `hostname,hostname2`). May be given more than once.
    #[arg(short = 's', long = "sut", value_name = "HOSTS")]
    pub sut: Vec<Sut>,

    /// Override config `mtui.connection_timeout` (seconds).
    #[arg(short = 'w', long = "connection-timeout", value_name = "SECONDS")]
    pub connection_timeout: Option<u64>,

    /// Enable debugging output.
    #[arg(short = 'd', long = "debug")]
    pub debug: bool,

    /// Override the default config path.
    #[arg(short = 'c', long = "config", value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Control coloured output.
    #[arg(long = "color", value_enum, default_value_t = ColorArg::Auto)]
    pub color: ColorArg,

    /// Gitea access token.
    #[arg(short = 'g', long = "gitea-token", value_name = "TOKEN")]
    pub gitea_token: Option<String>,

    /// OBS request review id, run under the automatic workflow
    /// (example: `SUSE:Maintenance:1:1`).
    #[arg(
        short = 'a',
        long = "auto-review-id",
        value_name = "RequestReviewID",
        group = "update_id"
    )]
    pub auto_review_id: Option<UpdateID>,

    /// OBS kernel/live-patch request review id, run under the kernel workflow
    /// (example: `SUSE:Maintenance:1:1`).
    #[arg(
        short = 'k',
        long = "kernel-review-id",
        value_name = "RequestReviewID",
        group = "update_id"
    )]
    pub kernel_review_id: Option<UpdateID>,
}

impl Args {
    /// Resolves the mutually-exclusive `-a`/`-k` pair into the selected update
    /// and its [`Workflow`], or `None` when neither was given.
    ///
    /// The two flags share a clap [`ArgGroup`](clap::ArgGroup), so at most one is
    /// ever set; upstream's `add_mutually_exclusive_group` maps to that.
    #[must_use]
    pub fn update(&self) -> Option<Update> {
        match (&self.auto_review_id, &self.kernel_review_id) {
            (Some(id), _) => Some(Update {
                id: id.clone(),
                workflow: Workflow::Auto,
            }),
            (_, Some(id)) => Some(Update {
                id: id.clone(),
                workflow: Workflow::Kernel,
            }),
            (None, None) => None,
        }
    }
}

/// A selected update: the parsed [`UpdateID`] plus the [`Workflow`] the
/// selecting flag implies (`-a` → [`Workflow::Auto`], `-k` → [`Workflow::Kernel`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// The update identifier (parsed RRID).
    pub id: UpdateID,
    /// The workflow the selecting flag seeds onto the loaded report.
    pub workflow: Workflow,
}

/// The `--color` choice, before it is resolved against the terminal.
///
/// [`Auto`](Self::Auto) mirrors upstream's default; turning it into a concrete
/// [`ColorMode`](crate::display::ColorMode) (TTY + `NO_COLOR` detection) is the
/// consumer's job, not the parser's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ColorArg {
    /// Colour iff stderr is a TTY and `NO_COLOR` is unset. The default.
    #[default]
    Auto,
    /// Always emit colour escapes.
    Always,
    /// Never emit colour escapes.
    Never,
}

/// A comma-separated SUT (System Under Test) host override.
///
/// Port of upstream `mtui.support.misc.SUTParse`: `"a,b,c"` becomes the argv
/// fragment `-t a -t b -t c` that the `add host` command consumes. The split
/// happens on construction; [`print_args`](Self::print_args) renders the stored
/// fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sut {
    hosts: Vec<String>,
}

impl Sut {
    /// Renders the `-t <host>`-joined argv fragment, matching upstream
    /// `SUTParse.print_args` (space-joined, one `-t` per host).
    #[must_use]
    pub fn print_args(&self) -> String {
        self.hosts
            .iter()
            .map(|h| format!("-t {h}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The parsed host tokens, in order.
    #[must_use]
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }
}

impl FromStr for Sut {
    type Err = std::convert::Infallible;

    /// Splits on `,`, mirroring upstream `args.split(",")`. Upstream keeps every
    /// token verbatim (it never trims or drops empties), so this does too — a
    /// trailing comma yields an empty host token exactly as upstream would.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self {
            hosts: s.split(',').map(str::to_owned).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_types::error::RridParseError;

    /// Parses argv (program name prepended) the way tests want: no process exit.
    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        let mut full = vec!["mtui"];
        full.extend_from_slice(argv);
        Args::try_parse_from(full)
    }

    #[test]
    fn no_args_leaves_everything_default() {
        let a = parse(&[]).unwrap();
        assert!(a.template_dir.is_none());
        assert!(a.sut.is_empty());
        assert!(a.connection_timeout.is_none());
        assert!(!a.debug);
        assert!(a.config.is_none());
        assert_eq!(a.color, ColorArg::Auto);
        assert!(a.gitea_token.is_none());
        assert!(a.update().is_none());
    }

    #[test]
    fn scalar_flags_parse() {
        let a = parse(&[
            "--template-dir",
            "/tmp/tpl",
            "--connection-timeout",
            "42",
            "--config",
            "/etc/mtui.toml",
            "--gitea-token",
            "abc123",
            "--debug",
        ])
        .unwrap();
        assert_eq!(a.template_dir.unwrap().to_str().unwrap(), "/tmp/tpl");
        assert_eq!(a.connection_timeout.unwrap(), 42);
        assert_eq!(a.config.unwrap().to_str().unwrap(), "/etc/mtui.toml");
        assert_eq!(a.gitea_token.unwrap(), "abc123");
        assert!(a.debug);
    }

    #[test]
    fn short_flags_alias_long() {
        let a = parse(&["-t", "/tmp/tpl", "-w", "7", "-c", "/c", "-g", "tok", "-d"]).unwrap();
        assert_eq!(a.template_dir.unwrap().to_str().unwrap(), "/tmp/tpl");
        assert_eq!(a.connection_timeout.unwrap(), 7);
        assert_eq!(a.config.unwrap().to_str().unwrap(), "/c");
        assert_eq!(a.gitea_token.unwrap(), "tok");
        assert!(a.debug);
    }

    #[test]
    fn sut_accumulates_across_repeats() {
        let a = parse(&["-s", "a,b", "-s", "c"]).unwrap();
        assert_eq!(a.sut.len(), 2);
        assert_eq!(a.sut[0].hosts(), ["a", "b"]);
        assert_eq!(a.sut[1].hosts(), ["c"]);
    }

    #[test]
    fn color_choices_parse_and_reject() {
        assert_eq!(
            parse(&["--color", "always"]).unwrap().color,
            ColorArg::Always
        );
        assert_eq!(parse(&["--color", "never"]).unwrap().color, ColorArg::Never);
        assert_eq!(parse(&["--color", "auto"]).unwrap().color, ColorArg::Auto);
        assert!(parse(&["--color", "sometimes"]).is_err());
    }

    #[test]
    fn auto_review_id_selects_auto_workflow() {
        let a = parse(&["-a", "SUSE:Maintenance:1:1"]).unwrap();
        let u = a.update().unwrap();
        assert_eq!(u.workflow, Workflow::Auto);
        assert_eq!(u.id.to_string(), "SUSE:Maintenance:1:1");
    }

    #[test]
    fn kernel_review_id_selects_kernel_workflow() {
        let a = parse(&["--kernel-review-id", "SUSE:Maintenance:2:3"]).unwrap();
        let u = a.update().unwrap();
        assert_eq!(u.workflow, Workflow::Kernel);
        assert_eq!(u.id.to_string(), "SUSE:Maintenance:2:3");
    }

    #[test]
    fn auto_and_kernel_are_mutually_exclusive() {
        let err = parse(&["-a", "SUSE:Maintenance:1:1", "-k", "SUSE:Maintenance:2:2"])
            .expect_err("both update selectors must conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn malformed_rrid_surfaces_types_parse_error() {
        // clap wraps the value-parser error; the source chain carries the
        // mtui-types RridParseError, keeping the interop contract's message.
        let err = parse(&["-a", "not-an-rrid"]).expect_err("malformed RRID must fail");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
        let has_rrid_err = std::error::Error::source(&err)
            .and_then(|s| s.downcast_ref::<RridParseError>())
            .is_some();
        assert!(has_rrid_err, "expected RridParseError in the source chain");
    }

    #[test]
    fn unknown_flag_is_rejected() {
        assert!(parse(&["--nope"]).is_err());
    }

    #[test]
    fn sut_print_args_matches_upstream_format() {
        let s: Sut = "a,b,c".parse().unwrap();
        assert_eq!(s.print_args(), "-t a -t b -t c");
    }

    #[test]
    fn sut_single_host_has_no_comma() {
        let s: Sut = "only".parse().unwrap();
        assert_eq!(s.print_args(), "-t only");
        assert_eq!(s.hosts(), ["only"]);
    }

    #[test]
    fn sut_trailing_comma_keeps_empty_token_like_upstream() {
        // Upstream `"a,".split(",")` -> `["a", ""]`; we preserve that verbatim.
        let s: Sut = "a,".parse().unwrap();
        assert_eq!(s.hosts(), ["a", ""]);
        assert_eq!(s.print_args(), "-t a -t ");
    }

    #[test]
    fn color_arg_default_is_auto() {
        assert_eq!(ColorArg::default(), ColorArg::Auto);
    }

    #[test]
    fn update_struct_equality() {
        let a = parse(&["-a", "SUSE:Maintenance:1:1"])
            .unwrap()
            .update()
            .unwrap();
        let b = Update {
            id: UpdateID::parse("SUSE:Maintenance:1:1").unwrap(),
            workflow: Workflow::Auto,
        };
        assert_eq!(a, b);
    }
}
