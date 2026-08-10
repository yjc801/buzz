//! `RUST_LOG` handed to a spawned managed-agent harness.
//!
//! `buzz_acp=info` alone is not enough, and the reason is easy to miss:
//! `EnvFilter` matches on an event's *target*, which defaults to the module
//! path — but buzz-acp sets an explicit `target:` on its diagnostic lines, and
//! an explicit target **replaces** the module path rather than extending it.
//! None of those targets begin with `buzz_acp`, so none of them ever matched.
//!
//! That silenced 19 targets under five roots: `pool::` (prompt, session,
//! model, permission, metrics), `acp::` (wire, update, usage, permission,
//! tool, cancel, thought, stream, session, plan, init), `canvas::fetch`,
//! `engram::core`, and `observer`. Among the casualties were the only records
//! of session rotation ("created session … for channel …") and of a
//! model-override miss — so two questions that logs exist to answer were
//! unanswerable from the log.
//!
//! Local agents only. A provider-backed agent's harness is launched by its
//! backend from a separate env, never through this path, and
//! `get_managed_agent_log` refuses remote agents outright.

/// Default directives, as `(target, level)` so they can be merged against an
/// operator's `RUST_LOG` per target rather than blindly appended.
///
/// `info` is deliberate rather than incidental: across these targets the call
/// sites are ~11 `debug`, 7 `info`, 6 `warn`, 2 `error`. At `info` the debug
/// lines stay off — including the 11 chatty `acp::wire` frame dumps and
/// `acp::thought` — so this surfaces the lines worth reading without inflating
/// every agent's log.
///
/// `acp::stream` is the exception that has to be named: it is the one
/// info-level site that fires per *chunk*, logging the text of every
/// `agent_message_chunk` (`buzz-acp/src/acp.rs`), so `acp=info` alone would
/// copy every agent response into the runtime log verbatim. That log is opened
/// once at spawn and appended to for the life of the process, and the 10 MB
/// rotation check runs only in `open_log_file` — so nothing would bound it
/// before the next restart. Operators who want the stream can ask for it (see
/// [`child_rust_log_filter_from`]).
const CHILD_LOG_DEFAULTS: &[(&str, &str)] = &[
    ("buzz_acp", "info"),
    ("pool", "info"),
    ("acp", "info"),
    ("acp::stream", "off"),
    ("engram", "info"),
    ("canvas", "info"),
    ("observer", "info"),
];

/// `RUST_LOG` for the spawned harness, derived from this process's own.
pub(super) fn child_rust_log_filter() -> String {
    child_rust_log_filter_from(std::env::var("RUST_LOG").ok().as_deref())
}

/// Env-free half, so the rule is testable without mutating process state that
/// parallel tests share.
///
/// Defaults are *merged*, not appended. Appending is not neutral: `EnvFilter`
/// keeps one directive per (target, span, fields) and a later duplicate
/// replaces the earlier one regardless of level, so a trailing `pool=info`
/// would silently re-enable an operator's `RUST_LOG=pool=off`. A default is
/// therefore dropped when the operator already names its target or an ancestor
/// of it — `acp=debug` suppresses both `acp=info` and `acp::stream=off`, and
/// `acp::stream=trace` suppresses only the latter, leaving `acp=info` to cover
/// the sibling targets the operator said nothing about.
///
/// Two coarser rules sit on top. Naming `buzz_acp` anywhere hands the operator
/// the whole filter untouched — the escape hatch for narrowing to one target
/// without this function widening it back out. And a bare level (`RUST_LOG=warn`)
/// names no target, so it does not suppress anything; the harness defaults
/// still apply to their own targets.
fn child_rust_log_filter_from(existing: Option<&str>) -> String {
    let existing = existing.map(str::trim).filter(|s| !s.is_empty());
    let Some(existing) = existing else {
        return render(CHILD_LOG_DEFAULTS);
    };
    if existing.contains("buzz_acp") {
        return existing.to_string();
    }

    let claimed: Vec<&str> = existing.split(',').filter_map(directive_target).collect();
    let merged: Vec<(&str, &str)> = CHILD_LOG_DEFAULTS
        .iter()
        .copied()
        .filter(|(target, _)| !claimed.iter().any(|c| covers(c, target)))
        .collect();
    if merged.is_empty() {
        return existing.to_string();
    }
    format!("{existing},{}", render(&merged))
}

fn render(directives: &[(&str, &str)]) -> String {
    directives
        .iter()
        .map(|(target, level)| format!("{target}={level}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// `true` when a directive on `claimed` already speaks for `target` — either
/// the same target or an ancestor of it (`acp` covers `acp::stream`).
fn covers(claimed: &str, target: &str) -> bool {
    target == claimed
        || (target.len() > claimed.len()
            && target.starts_with(claimed)
            && target[claimed.len()..].starts_with("::"))
}

/// The target a single `RUST_LOG` directive names, or `None` for the forms
/// that name no target (a bare level, or a span-only `[span]=level`).
///
/// The `[span{field=value}]` section is stripped first because it can itself
/// contain `=`, which would otherwise swallow part of it into the target.
fn directive_target(directive: &str) -> Option<&str> {
    let head = directive.split('[').next().unwrap_or_default();
    let target = head.split('=').next().unwrap_or_default().trim();
    if target.is_empty() || is_level(target) {
        return None;
    }
    Some(target)
}

/// `true` for the tokens `EnvFilter` reads as a level rather than a target.
fn is_level(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "off" | "error" | "warn" | "info" | "debug" | "trace" | "0" | "1" | "2" | "3" | "4" | "5"
    )
}

#[cfg(test)]
mod tests {
    use super::child_rust_log_filter_from;
    use std::str::FromStr;
    use tracing::Level;
    use tracing_subscriber::filter::Targets;

    /// Every root buzz-acp sets an explicit `target:` under. A target replaces
    /// the module path rather than extending it, so `buzz_acp=*` matches none
    /// of them and each root must be named.
    const HARNESS_TARGET_ROOTS: &[&str] = &["pool", "acp", "engram", "canvas", "observer"];

    /// Resolve `target` at `level` through the parser, rather than asserting on
    /// the string. The bug this module exists to fix was a filter that read
    /// correctly and matched nothing, so the string is not evidence.
    ///
    /// `Targets` is a newtype over the same `DirectiveSet<StaticDirective>`
    /// that backs `EnvFilter`'s static directives, parsed the same way and
    /// resolved most-specific-target-first the same way — so for the field-free
    /// directives this module emits, it answers exactly what `EnvFilter` would.
    /// It is used here because `EnvFilter::enabled` wants a `Metadata` and a
    /// `Context`, neither of which a unit test can honestly build.
    fn enabled(filter: &str, target: &str, level: Level) -> bool {
        Targets::from_str(filter)
            .unwrap_or_else(|e| panic!("filter {filter:?} does not parse: {e}"))
            .would_enable(target, &level)
    }

    #[test]
    fn default_filter_enables_every_harness_target_root() {
        let filter = child_rust_log_filter_from(None);
        for root in HARNESS_TARGET_ROOTS {
            let target = format!("{root}::anything");
            assert!(
                enabled(&filter, &target, Level::INFO),
                "target {target:?} is dropped by {filter:?}"
            );
        }
        // A bare root target (`observer`) has to work too, not just `root::x`.
        assert!(enabled(&filter, "observer", Level::INFO), "{filter}");
        // Lines that DON'T override target: fall back to the module path,
        // which does start with buzz_acp.
        assert!(enabled(&filter, "buzz_acp::pool", Level::INFO), "{filter}");
    }

    #[test]
    fn harness_targets_are_enabled_at_info_not_debug() {
        // Deliberate: ~11 of these call sites are debug!, including the
        // acp::wire frame dumps. Enabling debug would bloat every agent log.
        let filter = child_rust_log_filter_from(None);
        assert!(!enabled(&filter, "acp::wire", Level::DEBUG), "{filter}");
        assert!(!enabled(&filter, "pool::prompt", Level::DEBUG), "{filter}");
        assert!(enabled(&filter, "acp::wire", Level::WARN), "{filter}");
    }

    #[test]
    fn response_chunks_are_off_by_default() {
        // acp::stream logs the text of every agent_message_chunk at info. The
        // runtime log is opened once at spawn and only rotated there, so this
        // would grow unbounded for the life of a long-running agent.
        let filter = child_rust_log_filter_from(None);
        assert!(!enabled(&filter, "acp::stream", Level::INFO), "{filter}");
        // Off means off, not merely below info.
        assert!(!enabled(&filter, "acp::stream", Level::ERROR), "{filter}");
        // Its siblings under the same root stay on.
        assert!(enabled(&filter, "acp::tool", Level::INFO), "{filter}");
    }

    #[test]
    fn an_empty_or_blank_rust_log_is_treated_as_unset() {
        let expected = child_rust_log_filter_from(None);
        assert_eq!(child_rust_log_filter_from(Some("")), expected);
        assert_eq!(child_rust_log_filter_from(Some("   \t ")), expected);
    }

    #[test]
    fn an_unrelated_rust_log_is_extended_not_replaced() {
        let filter = child_rust_log_filter_from(Some("hyper=warn"));
        assert!(!enabled(&filter, "hyper::client", Level::INFO), "{filter}");
        assert!(enabled(&filter, "hyper::client", Level::WARN), "{filter}");
        assert!(enabled(&filter, "pool::session", Level::INFO), "{filter}");
    }

    #[test]
    fn an_explicit_buzz_acp_filter_is_passed_through_untouched() {
        // The operator escape hatch: someone debugging one target must be able
        // to narrow the filter without this function widening it back out.
        let operator = "buzz_acp=trace,pool::model=trace";
        assert_eq!(
            child_rust_log_filter_from(Some(operator)),
            operator,
            "an explicit buzz_acp filter must win outright"
        );
    }

    #[test]
    fn an_operator_silenced_root_stays_silenced() {
        // Appending `pool=info` after `pool=off` would win: EnvFilter keeps one
        // directive per target and the later duplicate replaces the earlier.
        let filter = child_rust_log_filter_from(Some("pool=off"));
        assert!(!enabled(&filter, "pool::session", Level::ERROR), "{filter}");
        // Roots the operator said nothing about are still covered.
        assert!(enabled(&filter, "canvas::fetch", Level::INFO), "{filter}");
    }

    #[test]
    fn an_operator_raised_root_is_not_downgraded() {
        // `acp=debug` must reach debug, and must not be clipped by the
        // acp::stream default the operator did not ask for.
        let filter = child_rust_log_filter_from(Some("acp=debug"));
        assert!(enabled(&filter, "acp::wire", Level::DEBUG), "{filter}");
        assert!(enabled(&filter, "acp::stream", Level::DEBUG), "{filter}");
    }

    #[test]
    fn asking_for_the_stream_gets_the_stream_and_keeps_the_rest() {
        // A leaf directive suppresses only the leaf default; `acp=info` still
        // covers the sibling targets the operator said nothing about.
        let filter = child_rust_log_filter_from(Some("acp::stream=trace"));
        assert!(enabled(&filter, "acp::stream", Level::TRACE), "{filter}");
        assert!(enabled(&filter, "acp::tool", Level::INFO), "{filter}");
        assert!(!enabled(&filter, "acp::tool", Level::DEBUG), "{filter}");
    }

    #[test]
    fn a_bare_level_names_no_target_and_suppresses_nothing() {
        let filter = child_rust_log_filter_from(Some("warn"));
        assert!(enabled(&filter, "pool::session", Level::INFO), "{filter}");
        assert!(enabled(&filter, "hyper", Level::WARN), "{filter}");
        assert!(!enabled(&filter, "hyper", Level::INFO), "{filter}");
    }

    #[test]
    fn a_span_field_directive_does_not_confuse_target_parsing() {
        // The `[span{field=value}]` section contains `=`; splitting on `=`
        // first would read the target as `pool[work{id`.
        let filter = child_rust_log_filter_from(Some("pool[work{id=7}]=off"));
        assert!(
            !filter.contains("pool=info"),
            "the operator named `pool`, so the default must not be appended: {filter}"
        );
        // A span-only directive names no target and suppresses nothing.
        // Asserted on the string rather than through `enabled`: span syntax is
        // an `EnvFilter` dynamic directive, which `Targets` does not model.
        let filter = child_rust_log_filter_from(Some("[work]=debug"));
        assert!(filter.starts_with("[work]=debug,"), "{filter}");
        assert!(filter.contains("pool=info"), "{filter}");
    }
}
