# Auto-Provisioned MCP — Manual Test Plan

Tests for #128: koda-ast extraction + capability registry + auto-connect.

## Prerequisites

```bash
# Build koda-ast binary
cargo build -p koda-ast --release
```

---

## Test 1: koda-ast works as standalone MCP server

**What**: Verify the MCP server starts, reports tools, and handles requests.

```bash
# Send MCP initialize request via stdin
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}' | timeout 5 ./target/release/koda-ast 2>/dev/null | head -1 | python3 -m json.tool
```

**Expected**: JSON response with `server_info.name = "koda-ast"` and `capabilities.tools`.

---

## Test 2: koda-ast returns tool list

```bash
# Send initialize + tools/list
(echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}'
echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
echo '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}') | timeout 5 ./target/release/koda-ast 2>/dev/null
```

**Expected**: Response with `AstAnalysis` in the tools list.

---

## Test 3: koda-ast analyzes a file

Create a test file:
```bash
echo 'fn main() { helper(); }\nfn helper() {}' > /tmp/test_ast.rs
```

```bash
(echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}'
echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"AstAnalysis","arguments":{"action":"analyze_file","file_path":"/tmp/test_ast.rs"}}}') | timeout 5 ./target/release/koda-ast 2>/dev/null
```

**Expected**: Response containing `main` and `helper` in the AST summary.

---

## Test 4: Auto-provision when koda-ast is on PATH

```bash
# Put koda-ast on PATH
export PATH="$(pwd)/target/release:$PATH"
which koda-ast  # Should print the path

# Run koda and ask for AST analysis
cargo run -p koda-cli -- -p "Use AstAnalysis to analyze the file koda-core/src/lib.rs"
```

**Expected**: Koda auto-connects to koda-ast MCP server and returns AST analysis.
Look for log: "Auto-provisioned MCP server 'koda-ast' for tool 'AstAnalysis'"

---

## Test 5: Helpful error when koda-ast is NOT on PATH

```bash
# Remove koda-ast from PATH (use a clean PATH)
PATH=/usr/bin:/bin cargo run -p koda-cli -- -p "Use AstAnalysis to analyze lib.rs"
```

**Expected**: Error message like:
```
Tool 'AstAnalysis' is available via the 'koda-ast' MCP server,
but 'koda-ast' is not installed.
Install: cargo install koda-ast
```

---

## Test 6: Capability registry unit tests

```bash
cargo test -p koda-core -- capability_registry
```

**Expected**: 2 tests pass (find_ast_analysis, unknown_tool).

---

## Test 7: koda-ast unit tests

```bash
cargo test -p koda-ast
```

**Expected**: 4 tests pass (analyze_rust, analyze_python, unsupported_ext, call_graph).

---

## Test 8: Full workspace tests still pass

```bash
cargo test --workspace --features koda-core/test-support
```

**Expected**: 420+ tests pass, 0 failures.
