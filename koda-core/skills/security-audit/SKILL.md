---
name: security-audit
description: Security vulnerability scan — finds vulnerabilities before attackers do
tags: [security, audit, vulnerabilities, owasp]
---

# Security Audit

You are performing a paranoid security audit. Find vulnerabilities before
attackers do.

## Principles
- **Discover the stack first.** Read the project structure and manifests to
  understand what you're auditing.
- Assume all input is hostile.
- Assume all dependencies have CVEs you haven't found yet.
- Trust nothing. Verify everything.

## Process
1. `List` the project to identify entry points and attack surface
2. Detect the language/framework and identify the appropriate dependency audit tool
3. `Grep` for vulnerability patterns systematically:
   - Secrets: `password`, `secret`, `api_key`, `token`, `private_key`, `BEGIN RSA`
   - Injection: `eval`, `exec`, `system`, `popen`, `subprocess`
   - SQL: raw string queries, string concatenation in queries
   - Crypto: `md5`, `sha1`, `DES`, `ECB`, weak random
   - File I/O: user-controlled paths, `..` traversal
4. `Read` auth flows, input handlers, and data processing code
5. `Bash` to check dependencies using whatever audit tool the project has
6. Compile findings by severity with CWE references

## Audit Checklist
1. **Injection**: SQL, command, path traversal, template, header injection
2. **Auth & Access Control**: Broken auth, missing access checks, privilege escalation
3. **Secrets**: Hardcoded keys, passwords, tokens in source or config
4. **Dependencies**: Check lock files for known CVEs
5. **Data Exposure**: Sensitive data in logs, error messages leaking internals
6. **Cryptography**: Weak algorithms, hardcoded IVs/salts
7. **Input Validation**: Missing validation, improper sanitization
8. **Network**: SSRF, open redirects, insecure TLS, CORS misconfiguration
9. **File System**: Symlink attacks, temp file races, directory traversal
10. **Concurrency**: Race conditions, TOCTOU bugs in security-critical paths

## Scope
- Prioritize: auth > data handling > input parsing > file I/O > everything else
- Large codebases: focus on attack surface (HTTP handlers, CLI parsers, file ops)
- Skip test files unless checking for secrets in fixtures
- NEVER modify code unless explicitly asked to fix something

## Output Format
- Severity: 🔴 Critical, 🟠 High, 🟡 Medium, 🔵 Low, 🟢 Pass
- Include CWE numbers where applicable
- Show vulnerable code and recommended fix
- End with executive summary: risk level, top 3 priorities, pass/fail
