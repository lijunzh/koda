//! Token audit: dump tool definitions for analysis.
//!
//! Migrated to `ToolCatalog` directly in #1265 item 5 PR-2 — we
//! only need definitions, not the full registry's FS / undo /
//! caps / proxy state. Construction is pure (no project root, no
//! skill discovery), making this test cheaper and clearer.

#[test]
fn dump_tool_definitions_for_audit() {
    let catalog = koda_core::tools::ToolCatalog::new();
    let defs = catalog.get_definitions(&[], &[]); // empty = all tools

    let mut total_chars = 0;
    let mut entries: Vec<(String, usize, usize)> = Vec::new();

    for def in &defs {
        let desc_chars = def.description.len();
        let param_chars = serde_json::to_string(&def.parameters).unwrap().len();
        total_chars += desc_chars + param_chars;
        entries.push((def.name.clone(), desc_chars, param_chars));
    }

    entries.sort_by_key(|e| std::cmp::Reverse(e.1 + e.2));

    eprintln!(
        "\n{:<20} {:>10} {:>10} {:>10}",
        "Tool", "Desc", "Params", "Total"
    );
    eprintln!("{}", "-".repeat(55));
    for (name, desc, params) in &entries {
        eprintln!(
            "{:<20} {:>10} {:>10} {:>10}",
            name,
            desc,
            params,
            desc + params
        );
    }
    eprintln!("{}", "-".repeat(55));
    eprintln!(
        "{:<20} {:>10} {:>10} {:>10}",
        "TOTAL",
        entries.iter().map(|e| e.1).sum::<usize>(),
        entries.iter().map(|e| e.2).sum::<usize>(),
        total_chars
    );
    eprintln!("Estimated tokens: ~{}", total_chars / 4);
    eprintln!("Tool count: {}", defs.len());
    eprintln!();

    // Also dump the full JSON for detailed review
    let json = serde_json::to_string_pretty(&defs).unwrap();
    eprintln!(
        "Full JSON size: {} chars, ~{} tokens",
        json.len(),
        json.len() / 4
    );
}
