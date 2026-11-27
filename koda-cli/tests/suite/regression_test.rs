//! Source-level guards for regressions that a behaviour test can't cheaply
//! reproduce: the TUI's redraw loop and how a sub-agent's trust is derived.

mod event_loop_structure {
    /// Regression test for the v0.1.11 frozen-input bug.
    ///
    /// The idle event loop in `run_event_loop` MUST call `self.draw()` before
    /// blocking on `tokio::select!` for keyboard events. Without it the
    /// viewport never redraws after keystrokes and input appears frozen.
    ///
    /// This is a source-level structural assertion — the only reliable way to
    /// guard against accidental removal during refactors.
    #[test]
    fn draw_called_before_idle_select_in_event_loop() {
        let source = include_str!("../../../koda-tui/src/tui_context/mod.rs");

        // Locate run_event_loop
        let fn_start = source
            .find("async fn run_event_loop")
            .expect("run_event_loop function not found in tui_context.rs");
        let body = &source[fn_start..];

        // Find the idle select! block (the one with crossterm_events.next())
        let select_marker = "self.crossterm_events.next()";
        let select_pos = body
            .find(select_marker)
            .expect("idle select! with crossterm_events.next() not found in run_event_loop");

        // self.draw() must appear (uncommented) between fn start and the select!
        let before_select = &body[..select_pos];
        let has_active_draw = before_select.lines().any(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with("//") && trimmed.contains("self.draw()")
        });
        assert!(
            has_active_draw,
            "self.draw() must be called (uncommented) before the idle tokio::select! in \
             run_event_loop. Without it the viewport never redraws and input appears \
             frozen (v0.1.11 bug)."
        );
    }

    /// #1354: the bg-activity overlay self-perpetuates its draw
    /// loop via `schedule_frame_in(1 s)` so the age column ticks
    /// and the activity pill stays live without depending on user
    /// keystrokes or new engine events. Pre-fix, `schedule_frame_in`
    /// was marked `#[allow(dead_code)]` with a TODO and the pill
    /// froze for minutes between sub-agent tool calls.
    ///
    /// Source-level guard: both draw paths (idle + inference) must
    /// schedule the next tick when there's non-terminal bg work.
    #[test]
    fn both_draw_paths_self_perpetuate_when_bg_work_exists() {
        let idle_src = include_str!("../../../koda-tui/src/tui_context/mod.rs");
        let inference_src = include_str!("../../../koda-tui/src/inference/select_loop.rs");

        for (label, src) in [("idle", idle_src), ("inference", inference_src)] {
            assert!(
                src.contains("schedule_frame_in"),
                "#1354 regression: {label} draw path must call \
                 frame_requester.schedule_frame_in(...) when child_activity_total > 0 \
                 so the bg-activity pill ticks between engine events. Without this \
                 the pill freezes the moment the sub-agent stops emitting tool calls."
            );
        }

        // Idle select! must consume the resulting notifications via
        // `draw_rx.recv()` \u2014 otherwise schedule_frame_in queues a
        // notification nobody reads, and the loop only wakes on
        // keystrokes anyway (the original bug presentation).
        let body_start = idle_src
            .find("async fn run_event_loop")
            .expect("run_event_loop not found");
        // Bound the search to `run_event_loop`'s body \u2014 the next
        // `\n    pub `/`\n    fn `/`\n    async fn ` at the same
        // 4-space indent marks the next item in the impl block.
        let body = &idle_src[body_start..];
        let body_end = body[1..]
            .find("\n    pub ")
            .or_else(|| body[1..].find("\n    fn "))
            .or_else(|| body[1..].find("\n    async fn "))
            .map(|i| i + 1)
            .unwrap_or(body.len());
        let run_event_loop_body = &body[..body_end];
        assert!(
            run_event_loop_body.contains("draw_rx.recv()"),
            "#1354 regression: idle tokio::select! must include a \
             `Some(()) = self.draw_rx.recv()` arm so frame-scheduler \
             notifications wake the loop and trigger the next \
             `self.draw()` at the top of the iteration. Without it the \
             schedule_frame_in tick has no consumer and the bg-activity \
             pill freezes between keystrokes."
        );
    }
}

/// **#1022 B19** \u{2014} sub-agent dispatch must clamp child trust against
/// the **runtime** trust mode (the `mode` parameter), not the
/// **stale startup** value `parent_config.trust`.
///
/// `cycle_trust` and `set_trust` mutate the `SharedTrustMode` atomic
/// but never the `KodaConfig.trust` field. Pre-fix, sub_agent_dispatch
/// passed `parent_config.trust` as the parent side of the clamp, so a
/// user who started in Auto and switched to Plan would still get sub-agents
/// clamped against Auto \u{2014} the child could run with broader privileges
/// than the parent's *current* mode. Real escalation.
///
/// The fix routes all three dispatch paths (fork/named/bg) through
/// `derive_child_trust(mode, declared)`. This test scans
/// `sub_agent_dispatch.rs` for the antipatterns
/// (`clamp(parent_config.trust,` or `derive_child_trust(parent_config.trust,`)
/// and fails with a clear pointer if either reappears.
///
/// Why structural and not e2e: an e2e test would need to (1) toggle
/// trust mid-session, (2) dispatch a sub-agent, (3) introspect the
/// child's effective trust. That's a heavy harness for a one-line
/// regression. The structural test catches the only way the bug
/// reintroduces.
mod sub_agent_dispatch_uses_runtime_trust_not_stale_config {
    use std::fs;
    use std::path::Path;

    #[test]
    fn no_clamp_or_derive_against_parent_config_trust() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let path = manifest_dir
            .join("..")
            .join("koda-core/src/sub_agent_dispatch.rs");
        let src =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));

        // Strip rustdoc + line comments before scanning so the
        // "antipattern this helper exists to prevent" prose in
        // doc comments doesn't trip the lint. Cheap pass: drop
        // anything from `//` to end of line. Block comments are
        // unused in this file but `///` is a `//` so it falls out
        // naturally.
        let code: String = src
            .lines()
            .map(|line| {
                if let Some(idx) = line.find("//") {
                    &line[..idx]
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        for needle in [
            "clamp(parent_config.trust",
            "derive_child_trust(parent_config.trust",
        ] {
            assert!(
                !code.contains(needle),
                "sub_agent_dispatch.rs uses `{needle}` \u{2014} this is the \
                 #1022 B19 antipattern. `parent_config.trust` is the \
                 stale startup value; runtime trust toggles never \
                 update it. Pass the runtime `mode` parameter instead. \
                 See `derive_child_trust` doc in `koda-core/src/trust.rs`."
            );
        }

        // Positive assertion: the helper *is* called somewhere.
        // Catches a refactor that drops trust derivation entirely.
        assert!(
            code.contains("derive_child_trust("),
            "sub_agent_dispatch.rs no longer calls `derive_child_trust` \
             at all \u{2014} child trust derivation has been bypassed. \
             Every fork/named/bg path must derive child trust through \
             this helper."
        );
    }
}
