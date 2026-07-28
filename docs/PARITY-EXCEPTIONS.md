# Greenlit parity-exception ledger

Parity exceptions are not defect waivers. They may cover only an intentional,
specification-permitted degradation or explicit non-goal. Every exception is
limited to one exact comparison case and one exact schema field. Wildcards,
prefixes, regular expressions, and whole-object exclusions are forbidden.
Rows are closed, never deleted.

| Exception ID | Case ID | Exact field | Authoritative source | Reason and scope | Owner approval | Removal criterion | Status |
|---|---|---|---|---|---|---|---|
| — | — | — | — | — | — | — | — |

## Field contract

- **Exception ID:** `GL-PARITY-NNN`, unique and never reused.
- **Case ID:** one exact committed comparison-case identifier.
- **Exact field:** one complete `ParityObservationV1` JSON path with no
  wildcard, pattern, prefix, or array-range syntax.
- **Authoritative source:** a GitHub documentation section or retained
  observed-behavior run.
- **Reason and scope:** the specification-permitted degradation or non-goal;
  an in-scope implementation bug is invalid.
- **Owner approval:** `Shane YYYY-MM-DD`; agent approval is invalid.
- **Removal criterion:** one concrete condition that makes the exception
  obsolete.
- **Status:** `active` or `closed`. Closed rows remain historical.
