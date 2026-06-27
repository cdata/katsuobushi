//! The shared rendering layer every subcommand emits through (design §13).
//!
//! Two output worlds live behind one [`Renderer`]: machine-readable `--json`
//! and polished human text. The renderer carries the two resolved policy bits —
//! `json` (was `--json` set?) and `color` (is decoration allowed *right now*?) —
//! so a subcommand never re-derives them; it just asks the renderer to
//! serialize a value or paint a string.
//!
//! ## Strict color gating (design §13, the acceptance core)
//!
//! Color resolution is a pure function, [`color_enabled`], over four injected
//! inputs — the `--color` choice, the `--json` flag, whether stdout is a TTY,
//! and whether `NO_COLOR` is present — so the policy is unit-testable without a
//! real terminal. The rules:
//!
//! - `--json` **always** wins: structured output is never decorated, so a parser
//!   never has to strip ANSI (and the emitted `start`/`attach` scripts in §13
//!   stay `exec`-clean).
//! - `always` forces color on, `never` forces it off.
//! - `auto` enables color **only** when stdout is a TTY *and* `NO_COLOR` is
//!   unset (the [NO_COLOR convention](https://no-color.org): any value, even
//!   empty, disables).
//!
//! Every color/glyph helper funnels through [`Renderer::color`]: when it is
//! `false` the helper returns the plain string with **zero** ANSI bytes. Glyphs
//! (`✓`/`⚠`) are plain Unicode and stay regardless — only the SGR coloring is
//! gated.
//!
//! Lands ahead of its callers (the subcommands migrate command-by-command,
//! design §12), so most of the surface here is `dead_code` until then.
#![allow(dead_code)]

use crate::{ColorWhen, Global};
use anyhow::Result;
use comfy_table::{ContentArrangement, Table};
use owo_colors::OwoColorize;
use serde::Serialize;
use std::io::IsTerminal;

/// Resolve whether human output may be colored, given the `--color` choice, the
/// `--json` flag, and the two environment probes (design §13). Pure over its
/// inputs so the gating matrix is testable without a real terminal — production
/// callers pass the live `stdout().is_terminal()` / `NO_COLOR` values via
/// [`Renderer::resolve`].
pub fn color_enabled(when: ColorWhen, json: bool, stdout_is_tty: bool, no_color: bool) -> bool {
    // `--json` is structured output: never decorated, whatever `--color` says.
    if json {
        return false;
    }
    match when {
        ColorWhen::Always => true,
        ColorWhen::Never => false,
        ColorWhen::Auto => stdout_is_tty && !no_color,
    }
}

/// One streamed agent report's flavor — the four `protocol::Status` variants,
/// mapped here to a glyph + color (design §13: `working`=dim, `done`=green ✓,
/// `blocked`=yellow ⚠, `info`=blue). Kept local rather than depending on
/// `katsuobushi-protocol`; the `prompt` subcommand maps `Status` → this when it
/// renders a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportKind {
    Working,
    Done,
    Blocked,
    Info,
}

/// The shared output handle: a subcommand emits either a typed value
/// (serialized in `--json` mode) or its human rendering through here, and paints
/// strings only when [`Self::color`] is set.
#[derive(Clone, Copy, Debug)]
pub struct Renderer {
    json: bool,
    color: bool,
}

impl Renderer {
    /// Build a renderer from already-resolved policy bits. Used directly by
    /// tests (which inject `color` rather than relying on the real terminal);
    /// production goes through [`Self::resolve`].
    pub fn new(json: bool, color: bool) -> Self {
        Self { json, color }
    }

    /// Resolve the renderer from the global flags against the live process
    /// environment — stdout's TTY-ness and `NO_COLOR` (design §13). This is the
    /// one place that touches the real terminal/env; the policy itself is the
    /// pure [`color_enabled`].
    pub fn resolve(global: Global) -> Self {
        let json = global.json;
        let color = color_enabled(
            global.color,
            json,
            std::io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
        );
        Self { json, color }
    }

    /// Whether `--json` mode is active.
    pub fn json(&self) -> bool {
        self.json
    }

    /// Whether decoration (color SGR codes) is currently permitted.
    pub fn color(&self) -> bool {
        self.color
    }

    /// Emit a value: in `--json` mode print its compact JSON serialization (one
    /// line — also the NDJSON unit for streamed `prompt` reports); otherwise
    /// print the human rendering produced by `human` (skipped when empty so a
    /// command can render "nothing" without a blank line).
    pub fn emit<T, F>(&self, value: &T, human: F) -> Result<()>
    where
        T: Serialize,
        F: FnOnce(&Self) -> String,
    {
        if self.json {
            println!("{}", serde_json::to_string(value)?);
        } else {
            let text = human(self);
            if !text.is_empty() {
                println!("{text}");
            }
        }
        Ok(())
    }

    // ---- color helpers (gated; plain string with zero ANSI when color off) ----

    /// Paint `text` green (or return it untouched when color is off).
    pub fn green(&self, text: &str) -> String {
        if self.color {
            text.green().to_string()
        } else {
            text.to_string()
        }
    }

    /// Paint `text` yellow when color is on.
    pub fn yellow(&self, text: &str) -> String {
        if self.color {
            text.yellow().to_string()
        } else {
            text.to_string()
        }
    }

    /// Paint `text` blue when color is on.
    pub fn blue(&self, text: &str) -> String {
        if self.color {
            text.blue().to_string()
        } else {
            text.to_string()
        }
    }

    /// Paint `text` red when color is on (used for the `error:` prefix).
    pub fn red(&self, text: &str) -> String {
        if self.color {
            text.red().to_string()
        } else {
            text.to_string()
        }
    }

    /// Dim `text` when color is on (used for `working` reports and subtle tags).
    pub fn dim(&self, text: &str) -> String {
        if self.color {
            text.dimmed().to_string()
        } else {
            text.to_string()
        }
    }

    /// Render one streamed report with its status glyph + color (design §13).
    /// The glyph is plain Unicode and survives gating; only the coloring is
    /// gated, so with color off this is the bare `"✓ text"` / `"⚠ text"` / text.
    pub fn report(&self, kind: ReportKind, text: &str) -> String {
        match kind {
            ReportKind::Working => self.dim(text),
            ReportKind::Done => self.green(&format!("✓ {text}")),
            ReportKind::Blocked => self.yellow(&format!("⚠ {text}")),
            ReportKind::Info => self.blue(text),
        }
    }

    /// Render a structured error: human form is the `error:` prefix (red when
    /// color is on) followed by the message; `--json` form is the
    /// `{"error": "...", "kind": "..."}` object the skill parses (design §13).
    /// Pure — does not print or exit, so it is unit-testable; [`Self::fail`]
    /// wires it to stderr + a nonzero exit.
    pub fn render_error(&self, kind: &str, message: &str) -> String {
        if self.json {
            serde_json::json!({ "error": message, "kind": kind }).to_string()
        } else {
            format!("{} {message}", self.red("error:"))
        }
    }

    /// Print a rendered error to stderr and exit nonzero (design §13: `--json`
    /// errors exit nonzero too). Minimal exit-code wiring — the real per-command
    /// error mapping grows as the subcommands land (design §12).
    pub fn fail(&self, kind: &str, message: &str) -> ! {
        eprintln!("{}", self.render_error(kind, message));
        std::process::exit(1);
    }
}

/// Render an aligned, borderless status table built on `comfy-table` — the
/// replacement for the old `column -t` (design §13; lib/sandbox/default.nix:1814).
/// Width is computed from content (arrangement disabled) rather than the
/// terminal, so output is deterministic for snapshot tests. The helper itself
/// emits no ANSI; a caller that wants a colored cell paints the string with a
/// [`Renderer`] helper before handing it in (so gating stays in one place).
pub fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(headers.to_vec());
    for row in rows {
        table.add_row(row.clone());
    }
    table.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// True iff the string carries an ANSI escape (SGR) sequence.
    fn has_ansi(s: &str) -> bool {
        s.contains('\u{1b}')
    }

    // ---- color gating matrix (the acceptance core) ----

    #[test]
    fn it_disables_color_in_json_mode_regardless_of_choice() {
        // --json wins over every --color choice and a live TTY.
        for when in [ColorWhen::Auto, ColorWhen::Always, ColorWhen::Never] {
            assert!(
                !color_enabled(when, true, true, false),
                "{when:?}: --json must force color off"
            );
        }
    }

    #[test]
    fn it_disables_color_when_stdout_is_not_a_tty_under_auto() {
        assert!(!color_enabled(ColorWhen::Auto, false, false, false));
    }

    #[test]
    fn it_disables_color_when_no_color_is_present_under_auto() {
        // NO_COLOR set (even though stdout is a TTY) → off.
        assert!(!color_enabled(ColorWhen::Auto, false, true, true));
    }

    #[test]
    fn it_enables_color_under_auto_on_a_tty_without_no_color() {
        assert!(color_enabled(ColorWhen::Auto, false, true, false));
    }

    #[test]
    fn it_forces_color_on_with_always_even_off_a_tty() {
        assert!(color_enabled(ColorWhen::Always, false, false, false));
    }

    #[test]
    fn it_forces_color_off_with_never_even_on_a_tty() {
        assert!(!color_enabled(ColorWhen::Never, false, true, false));
    }

    // ---- helpers emit zero ANSI when gating is off ----

    #[test]
    fn it_paints_plain_text_when_color_is_off() {
        let r = Renderer::new(false, false);
        for painted in [
            r.green("ok"),
            r.yellow("warn"),
            r.blue("note"),
            r.red("bad"),
            r.dim("quiet"),
        ] {
            assert!(!has_ansi(&painted), "no ANSI when color off: {painted:?}");
        }
        assert_eq!(r.green("ok"), "ok");
        assert_eq!(r.dim("quiet"), "quiet");
    }

    #[test]
    fn it_paints_with_ansi_when_color_is_on() {
        let r = Renderer::new(false, true);
        let painted = r.green("ok");
        assert!(
            has_ansi(&painted),
            "ANSI expected when color on: {painted:?}"
        );
        assert!(painted.contains("ok"), "text preserved: {painted:?}");
    }

    #[test]
    fn it_renders_report_glyphs_without_ansi_when_color_off() {
        let r = Renderer::new(false, false);
        assert_eq!(r.report(ReportKind::Working, "building"), "building");
        assert_eq!(r.report(ReportKind::Done, "shipped"), "✓ shipped");
        assert_eq!(r.report(ReportKind::Blocked, "need token"), "⚠ need token");
        assert_eq!(r.report(ReportKind::Info, "fyi"), "fyi");
        for kind in [
            ReportKind::Working,
            ReportKind::Done,
            ReportKind::Blocked,
            ReportKind::Info,
        ] {
            assert!(!has_ansi(&r.report(kind, "x")), "report {kind:?} plain");
        }
    }

    #[test]
    fn it_colors_report_glyphs_when_color_on() {
        let r = Renderer::new(true, true);
        let done = r.report(ReportKind::Done, "shipped");
        assert!(has_ansi(&done), "colored when on: {done:?}");
        // The glyph survives the coloring.
        assert!(done.contains("✓ shipped"), "glyph + text kept: {done:?}");
    }

    // ---- structured error rendering ----

    #[test]
    fn it_renders_a_human_error_with_a_plain_prefix_when_color_off() {
        let r = Renderer::new(false, false);
        let out = r.render_error("not_found", "no such instance");
        assert_eq!(out, "error: no such instance");
        assert!(!has_ansi(&out));
    }

    #[test]
    fn it_colors_the_error_prefix_when_color_on() {
        let r = Renderer::new(false, true);
        let out = r.render_error("not_found", "boom");
        assert!(has_ansi(&out), "prefix colored: {out:?}");
        assert!(out.contains("error:") && out.contains("boom"));
    }

    #[test]
    fn it_renders_a_json_error_object_with_message_and_kind() {
        let r = Renderer::new(true, false);
        let out = r.render_error("bad_spec", "version skew");
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(value["error"], "version skew");
        assert_eq!(value["kind"], "bad_spec");
        assert!(!has_ansi(&out), "json errors are never decorated");
    }

    // ---- the emit abstraction ----

    #[test]
    fn it_serializes_the_value_in_json_mode() {
        // emit() prints; assert the serialization path the human closure is not
        // invoked in json mode by panicking if it were.
        let r = Renderer::new(true, false);
        let value = serde_json::json!({ "name": "foo-1", "port": 20000 });
        r.emit(&value, |_| {
            panic!("human closure must not run in --json mode")
        })
        .expect("emit should serialize");
    }

    #[test]
    fn it_uses_the_human_closure_when_not_json() {
        let r = Renderer::new(false, false);
        let mut ran = false;
        r.emit(&serde_json::json!(null), |_| {
            ran = true;
            String::new() // empty → no print, but the closure still ran
        })
        .expect("emit should call the human closure");
        assert!(ran, "human closure runs when not --json");
    }

    // ---- table helper ----

    #[test]
    fn it_renders_a_plain_table_without_ansi() {
        let table = render_table(
            &["#", "INSTANCE", "STATE"],
            &[
                vec!["1".into(), "foo-abc".into(), "running".into()],
                vec!["2".into(), "bar-xyz".into(), "stopped".into()],
            ],
        );
        assert!(!has_ansi(&table), "comfy-table output carries no ANSI");
        assert!(table.contains("INSTANCE"), "header present: {table}");
        assert!(table.contains("foo-abc") && table.contains("bar-xyz"));
        // Borderless: no box-drawing glyphs from the NOTHING preset.
        assert!(!table.contains('│') && !table.contains('─'));
    }
}
