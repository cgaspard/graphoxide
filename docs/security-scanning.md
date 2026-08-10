# Security scanning and advisory exceptions

Graphoxide's security workflow reports dependency advisories on pull requests,
pushes to `main`, and a weekly schedule. It also runs CodeQL for Rust,
JavaScript/TypeScript, and GitHub Actions. The workflow has read-only repository
access except for the CodeQL analysis step's narrowly scoped permission to
upload security results.

Repository-level Dependabot vulnerability alerts are enabled. Automated
security fixes remain disabled, and the weekly GitHub Actions and npm update
pull requests are review-only: no dependency update is merged automatically.

## Locked dependency audits

Run the same advisory gate locally with:

```bash
cargo install --locked cargo-audit --version 0.22.2
npm run audit:security
```

The command audits three committed lockfiles:

| Scanner | Locked input |
| --- | --- |
| `cargo audit` | `Cargo.lock` |
| `npm audit` | `package-lock.json` |
| `npm audit` | `editors/vscode/package-lock.json` |

Both scanners contact their configured advisory source or package registry, so
the gate needs network access. Package installation scripts are disabled, and
the npm scans use only the existing package locks. The wrapper does not install,
update, or execute repository dependencies.

Scanner output is captured with a five-minute timeout and a 4 MiB limit per
scope. Reports must be bounded, valid UTF-8 JSON with the expected schema. Raw
reports, advisory descriptions, subprocess errors, registry responses, source
content, and credentials are not written to normal logs. The gate reports only
the scanner, lockfile, advisory ID, and package needed to identify an
unreviewed finding.

Policy parser tests are part of both repository verification modes and can be
run directly without scanners or network access:

```bash
npm run test:security
npm run verify:pre-push
npm run verify:release
```

## Exception policy

The reviewed exception file is
`.github/security/advisory-exceptions.json`. It begins with no exceptions. The
gate rejects malformed entries, unknown fields or scopes, duplicate entries,
expired entries, entries more than 30 days from the current UTC date, and valid
entries that no longer match a current finding. Findings without a usable
advisory ID cannot be excepted.

An exception must match one advisory, scanner, lockfile, and package exactly:

```json
{
  "advisory": "RUSTSEC-2026-0001",
  "scanner": "cargo-audit",
  "lockfile": "Cargo.lock",
  "package": "example-crate",
  "rationale": "The fixed release is being qualified before the dependency update.",
  "owner": "@github-owner",
  "trackingIssue": 123,
  "expires": "2026-08-30"
}
```

Required fields are:

- `advisory`: the scanner's `RUSTSEC`, `GHSA`, `CVE`, or numeric npm advisory
  identifier.
- `scanner` and `lockfile`: one of the three exact scopes in the table above.
- `package`: the affected package name reported by that scanner.
- `rationale`: a specific, printable explanation between 20 and 500 characters.
- `owner`: a GitHub handle beginning with `@` that owns remediation.
- `trackingIssue`: a positive issue number, `#`-prefixed number, or full GitHub
  issue URL.
- `expires`: a valid `YYYY-MM-DD` UTC date no more than 30 days away.

Before adding an exception, open a remediation issue, identify an owner, and
document why an immediate upgrade or removal is unsafe. Submit the smallest
possible exception in a reviewed pull request. Remove it when the finding is
fixed; the unused-entry check makes stale exceptions fail the next audit. An
extension requires a fresh review and may never move the expiration beyond 30
days from the day of that review.

Exceptions do not suppress CodeQL, hide raw findings in GitHub's security
products, merge dependency updates automatically, or authorize floating action
versions. See [CI and release dependency policy](ci-release-dependencies.md) for
the separate GitHub Actions update and pinning policy.
