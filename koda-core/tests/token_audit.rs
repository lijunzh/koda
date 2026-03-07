//! Token audit: dump tool definitions for analysis.

use std::path::PathBuf;

#[test]
fn dump_tool_definitions_for_audit() {
    let registry = koda_core::tools::ToolRegistry::new(PathBuf::from("."));
    let defs = registry.get_definitions(&[]); // empty = all tools

    let mut total_chars = 0;
    let mut entries: Vec<(String, usize, usize)> = Vec::new();

    for def in &defs {
        let desc_chars = def.description.len();
        let param_chars = serde_json::to_string(&def.parameters).unwrap().len();
        total_chars += desc_chars + param_chars;
        entries.push((def.name.clone(), desc_chars, param_chars));
    }

    entries.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));

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
