//! The process-wide `tracing` filter policy both entrypoints resolve through.
//!
//! It lives in `mtui-core` because `mtui-cli` and `mtui-mcp` are sibling leaves:
//! their only shared place for a *security* decision about logging is the
//! composition root, which makes "raising mtui's verbosity must not switch on
//! the HTTP transport's `DEBUG`" true by construction rather than true of
//! whichever entrypoint remembered.
//!
//! The policy is expressed as **directive strings**, not `EnvFilter` values, so
//! `mtui-core` need not depend on `tracing-subscriber` — the same invariant that
//! keeps subscriber types out of the lower crates. Each binary parses the
//! resolved string itself.

/// The third-party HTTP transport targets whose `DEBUG` output must not be
/// switched on as a side effect of raising *mtui's* own verbosity (#439).
///
/// `hyper-util`'s connection pool logs its pooled key at `DEBUG` — `(scheme,
/// authority)`, and an `http::uri::Authority` retains userinfo. `reqwest` strips
/// the first-hop userinfo at build time but never re-strips a redirect's
/// `Location: https://user:pass@host/…`, so a hostile redirect puts a
/// credential-shaped authority into that key.
///
/// `EnvFilter` matches a target as a raw `starts_with` prefix, so `hyper` alone
/// would already cover `hyper_util::client::legacy::pool`; `hyper_util` is named
/// anyway so the intent survives a rename or a narrowing of the `hyper` entry.
///
/// **These three are the whole surface, not a sample.** `h2` was checked and
/// does not belong: its `DEBUG` lines are frame shapes and protocol-error
/// breadcrumbs, and its `Debug for Headers` omits the header block
/// (h2-0.4.15 `src/frame/headers.rs`), so header-bearing lines are `TRACE` only.
pub const TRANSPORT_LOG_TARGETS: &[&str] = &["hyper_util", "hyper", "reqwest"];

/// The `EnvFilter` directives that cap [`TRANSPORT_LOG_TARGETS`] at `INFO`.
///
/// Appended to a `debug` directive string mtui builds for itself, and to a
/// `RUST_LOG` that would otherwise raise those targets (see
/// [`resolve_log_directives_from`]). `INFO` rather than `WARN`/`OFF` on purpose:
/// real transport warnings and errors must still reach the operator.
///
/// A literal, pinned as a literal by the tests, so a respelling `EnvFilter`'s
/// prefix match would silently absorb (`hyper-util` for `hyper_util`, swallowed
/// by the `hyper=info` entry) still shows up as a diff.
pub const TRANSPORT_LOG_CARVE_OUT: &str = "hyper_util=info,hyper=info,reqwest=info";

/// The one-line notice printed at startup when `RUST_LOG` explicitly opts the
/// HTTP transport into `DEBUG` — the one remaining way to get the credential
/// -shaped pool authority into the log, so it says so out loud.
pub const TRANSPORT_DEBUG_NOTICE: &str = "warn: RUST_LOG names a transport target at debug/trace: \
     hyper/hyper_util/reqwest may print connection-pool authorities, which a redirect can load \
     with credentials";

/// The resolved process log filter: the directive string to parse, plus the
/// startup notice to print (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogDirectives {
    /// The `EnvFilter` directive string the entrypoint should parse.
    pub directives: String,
    /// Set when `RUST_LOG` explicitly names a transport target at `debug`/
    /// `trace`, i.e. when the carve-out was deliberately stood down.
    pub transport_debug_opt_in: bool,
}

impl LogDirectives {
    /// The startup notice to print on stderr, or `None` when the transport is
    /// capped.
    ///
    /// Printed rather than `tracing::warn!`-ed on purpose: the triggering
    /// `RUST_LOG` is often target-scoped (`RUST_LOG=hyper_util=debug`), enabling
    /// no `mtui_*` target — a `WARN` would be filtered out by the very filter it
    /// warns about.
    #[must_use]
    pub fn notice(&self) -> Option<&'static str> {
        self.transport_debug_opt_in
            .then_some(TRANSPORT_DEBUG_NOTICE)
    }
}

/// Resolve the process log filter from `$RUST_LOG` and the entrypoint's own
/// defaults.
///
/// See [`resolve_log_directives_from`] for the policy; this only reads the
/// environment variable.
#[must_use]
pub fn resolve_log_directives(defaults: &str) -> LogDirectives {
    resolve_log_directives_from(std::env::var("RUST_LOG").ok().as_deref(), defaults)
}

/// Resolve the process log filter from an explicit `RUST_LOG` value and the
/// entrypoint's own defaults.
///
/// * `RUST_LOG` **unset** → `defaults` verbatim. The entrypoint has already put
///   [`TRANSPORT_LOG_CARVE_OUT`] where it belongs there.
/// * `RUST_LOG` **set** → the operator's directives, with
///   [`TRANSPORT_LOG_CARVE_OUT`] applied *on top* for every transport target the
///   operator did not name — but only when their directives carry a *global*
///   `debug`/`trace`, which is the only way an unnamed target reaches `DEBUG`.
///
/// The narrow trigger keeps the carve-out from becoming a *raise*: appending
/// `hyper=info` to `RUST_LOG=error`, `off` or `mtui_core=trace` would push the
/// transport **up** to `INFO`, above the level the operator chose. With a global
/// `debug`/`trace` present the added directive can only lower a target.
///
/// "Named" — which stands the cap down and sets [`LogDirectives::notice`] —
/// means the operator's target is a prefix of (or equal to) the transport's, the
/// only shape `EnvFilter`'s longest-match resolution lets a `<transport>=info`
/// cap *overrule*. So `RUST_LOG=hyper=debug` hands over the whole hyper stack
/// rather than having the cap fight the request, while
/// `RUST_LOG=debug,hyper_util=warn` still caps `hyper` and `reqwest` because a
/// `hyper=info` beside it cannot overrule the longer `hyper_util`.
#[must_use]
pub fn resolve_log_directives_from(rust_log: Option<&str>, defaults: &str) -> LogDirectives {
    let Some(user) = rust_log else {
        return LogDirectives {
            directives: defaults.to_owned(),
            transport_debug_opt_in: false,
        };
    };

    let mut verbose_global = false;
    let mut opt_in = false;
    // Which transport targets the operator's directives already speak for.
    // Sized from the list, not a literal, so a target added to
    // `TRANSPORT_LOG_TARGETS` cannot fall off a `zip` and stop being capped.
    let mut named = vec![false; TRANSPORT_LOG_TARGETS.len()];

    for chunk in user.split(',') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let (target, verbose) = parse_directive(chunk);
        let Some(target) = target else {
            // Only a target-less directive can reach an unmentioned target.
            verbose_global |= verbose;
            continue;
        };
        for (slot, transport) in named.iter_mut().zip(TRANSPORT_LOG_TARGETS) {
            // `EnvFilter` resolves an event through the *longest* matching
            // target, so a `<transport>=info` cap overrules the operator only
            // when their target is a prefix of (or equal to) the transport's.
            // That, and only that, stands the cap down — regardless of level,
            // since the raise direction is the one that surprises.
            *slot |= transport.starts_with(target);
            // Warning is the broader question: a *longer* target
            // (`hyper_util::client::legacy::pool=debug`) beats the cap for the
            // events it matches, so it is an opt-in even though the cap goes on.
            if verbose && (transport.starts_with(target) || target.starts_with(transport)) {
                opt_in = true;
            }
        }
    }

    let mut directives = user.trim().to_owned();
    if verbose_global {
        for (named, transport) in named.iter().zip(TRANSPORT_LOG_TARGETS) {
            if !named {
                directives.push(',');
                directives.push_str(transport);
                directives.push_str("=info");
            }
        }
    }

    LogDirectives {
        directives,
        transport_debug_opt_in: opt_in,
    }
}

/// Split one `EnvFilter` directive into `(target, level is more verbose than
/// INFO)`.
///
/// A `None` target means the directive applies to everything: a bare global
/// level (`debug`), or a span-only directive (`[span]=debug`), lumped in with
/// the globals because it too can match a transport event.
///
/// Deliberately lenient — it only decides whether to *add* a cap, and the binary
/// parses the real string afterwards, so a misread input is at worst an
/// unnecessary cap. The one direction that must not be wrong is reading a
/// verbose directive as non-verbose, hence "no explicit level" is `TRACE`, which
/// is what `EnvFilter` does with it.
fn parse_directive(chunk: &str) -> (Option<&str>, bool) {
    // `target[span{field=value}]=level`: the level follows the *last* `=`, and
    // only when level-shaped — otherwise the `=` belonged to a field filter.
    let (head, level) = match chunk.rsplit_once('=') {
        Some((head, level)) if is_level(level) => (head, Some(level)),
        _ => (chunk, None),
    };
    // A bare level-shaped directive is the *global* level, not a target.
    if level.is_none() && is_level(head) {
        return (None, is_verbose_level(head));
    }
    let verbose = level.is_none_or(is_verbose_level);
    let target = head.split(['[', '{']).next().unwrap_or_default().trim();
    if target.is_empty() {
        (None, verbose)
    } else {
        (Some(target), verbose)
    }
}

/// Whether the text is one of `EnvFilter`'s level tokens. The empty string
/// counts: `target=` is how `EnvFilter` spells "this target at `TRACE`".
fn is_level(text: &str) -> bool {
    text.is_empty()
        || matches!(
            text.to_ascii_lowercase().as_str(),
            "off"
                | "error"
                | "warn"
                | "info"
                | "debug"
                | "trace"
                | "0"
                | "1"
                | "2"
                | "3"
                | "4"
                | "5"
        )
}

/// Whether the level token is more verbose than `INFO`, i.e. whether it is one
/// that would print the transport's connection internals.
fn is_verbose_level(text: &str) -> bool {
    text.is_empty()
        || matches!(
            text.to_ascii_lowercase().as_str(),
            "debug" | "trace" | "4" | "5"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two spellings of one policy — the list drives the "did the operator name
    /// it?" test, the string drives the cap — so a target in only one is a hole.
    #[test]
    fn carve_out_string_caps_exactly_the_listed_targets() {
        let rebuilt = TRANSPORT_LOG_TARGETS
            .iter()
            .map(|t| format!("{t}=info"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(rebuilt, TRANSPORT_LOG_CARVE_OUT);
    }

    #[test]
    fn unset_rust_log_keeps_the_entrypoint_defaults_verbatim() {
        let resolved = resolve_log_directives_from(None, "debug,hyper=info");
        assert_eq!(resolved.directives, "debug,hyper=info");
        assert_eq!(resolved.notice(), None);
    }

    /// The headline case: `RUST_LOG=debug` names no transport target, so the
    /// cap goes on top of it.
    #[test]
    fn global_debug_gets_the_full_carve_out() {
        let resolved = resolve_log_directives_from(Some("debug"), "info");
        assert_eq!(
            resolved.directives,
            "debug,hyper_util=info,hyper=info,reqwest=info"
        );
        assert_eq!(resolved.notice(), None);
    }

    #[test]
    fn global_trace_gets_the_full_carve_out() {
        let resolved = resolve_log_directives_from(Some("trace"), "info");
        assert_eq!(
            resolved.directives,
            "trace,hyper_util=info,hyper=info,reqwest=info"
        );
    }

    /// An unrelated per-target directive must not read as a transport opt-in:
    /// the global `debug` riding beside it still gets the full cap.
    #[test]
    fn unrelated_target_directive_does_not_stand_the_carve_out_down() {
        let resolved = resolve_log_directives_from(Some("mtui_core=trace,debug"), "info");
        assert_eq!(
            resolved.directives,
            "mtui_core=trace,debug,hyper_util=info,hyper=info,reqwest=info"
        );
        assert_eq!(resolved.notice(), None);
    }

    /// No global directive enables no unnamed target, so there is nothing to cap
    /// — and appending one would *raise* `hyper`/`reqwest` to `INFO`.
    #[test]
    fn unrelated_target_directive_alone_adds_nothing() {
        let resolved = resolve_log_directives_from(Some("mtui_core=trace"), "info");
        assert_eq!(resolved.directives, "mtui_core=trace");
    }

    /// The same reasoning at the other end: a coarse global must never be
    /// pushed *up* to the cap's `INFO`.
    #[test]
    fn coarse_and_silent_globals_are_left_alone() {
        for coarse in ["error", "warn", "info", "off", "0", "3"] {
            let resolved = resolve_log_directives_from(Some(coarse), "info");
            assert_eq!(
                resolved.directives, coarse,
                "a {coarse} RUST_LOG must not be raised by the carve-out"
            );
            assert_eq!(resolved.notice(), None);
        }
    }

    /// The escape hatch: an explicit transport target is left exactly as
    /// written — and announced.
    #[test]
    fn explicit_transport_opt_in_is_untouched_and_announced() {
        for opt_in in [
            "hyper_util=debug",
            "hyper=debug",
            "reqwest=trace",
            "hyper_util::client::legacy::pool=debug",
            "hyper_util",
        ] {
            let resolved = resolve_log_directives_from(Some(opt_in), "info");
            assert_eq!(resolved.directives, opt_in);
            assert_eq!(
                resolved.notice(),
                Some(TRANSPORT_DEBUG_NOTICE),
                "{opt_in} is an informed opt-in and must say so"
            );
        }
    }

    /// `hyper=debug` asks for the whole hyper stack, so `hyper_util` counts as
    /// named too — otherwise the more specific `hyper_util=info` cap would
    /// silently overrule the request.
    #[test]
    fn a_shorter_prefix_names_the_longer_transport_target() {
        let resolved = resolve_log_directives_from(Some("debug,hyper=debug"), "info");
        assert_eq!(resolved.directives, "debug,hyper=debug,reqwest=info");
        assert_eq!(resolved.notice(), Some(TRANSPORT_DEBUG_NOTICE));
    }

    /// Why "named" is *not* symmetric: `hyper_util=warn` does not speak for
    /// `hyper`, so `hyper` is still capped — and being the shorter target, that
    /// `hyper=info` loses longest-match to the operator's own `hyper_util=warn`.
    #[test]
    fn naming_the_longer_target_does_not_shield_the_shorter_one() {
        let resolved = resolve_log_directives_from(Some("debug,hyper_util=warn"), "info");
        assert_eq!(
            resolved.directives,
            "debug,hyper_util=warn,hyper=info,reqwest=info"
        );
        assert_eq!(resolved.notice(), None);
    }

    /// A target *longer* than the transport's beats the cap for the events it
    /// matches — worth announcing, but the rest of the crate stays capped.
    #[test]
    fn a_longer_target_is_announced_and_still_takes_the_cap() {
        let resolved = resolve_log_directives_from(
            Some("debug,hyper_util::client::legacy::pool=debug"),
            "info",
        );
        assert_eq!(
            resolved.directives,
            "debug,hyper_util::client::legacy::pool=debug,hyper_util=info,hyper=info,reqwest=info"
        );
        assert_eq!(resolved.notice(), Some(TRANSPORT_DEBUG_NOTICE));
    }

    /// Naming a transport target to turn it *down* is not an opt-in: the other
    /// two are still capped and nothing is announced.
    #[test]
    fn a_transport_target_named_at_a_coarse_level_is_not_an_opt_in() {
        let resolved = resolve_log_directives_from(Some("debug,reqwest=warn"), "info");
        assert_eq!(
            resolved.directives,
            "debug,reqwest=warn,hyper_util=info,hyper=info"
        );
        assert_eq!(resolved.notice(), None);
    }

    #[test]
    fn empty_rust_log_stays_empty() {
        // `RUST_LOG=` disables everything; adding caps would switch the
        // transport back on at INFO.
        let resolved = resolve_log_directives_from(Some(""), "info");
        assert_eq!(resolved.directives, "");
        assert_eq!(resolved.notice(), None);
    }

    #[test]
    fn parse_directive_reads_targets_levels_and_implicit_trace() {
        assert_eq!(parse_directive("debug"), (None, true));
        assert_eq!(parse_directive("DEBUG"), (None, true));
        assert_eq!(parse_directive("info"), (None, false));
        assert_eq!(parse_directive("5"), (None, true));
        assert_eq!(
            parse_directive("mtui_core=trace"),
            (Some("mtui_core"), true)
        );
        assert_eq!(
            parse_directive("mtui_core=warn"),
            (Some("mtui_core"), false)
        );
        // No explicit level is `EnvFilter`'s TRACE.
        assert_eq!(parse_directive("mtui_core"), (Some("mtui_core"), true));
        assert_eq!(parse_directive("mtui_core="), (Some("mtui_core"), true));
        // Span/field syntax: the target is what precedes the bracket, and a
        // field's `=` is not a level.
        assert_eq!(
            parse_directive("hyper[conn{id=1}]=debug"),
            (Some("hyper"), true)
        );
        assert_eq!(parse_directive("[conn]=debug"), (None, true));
    }
}
