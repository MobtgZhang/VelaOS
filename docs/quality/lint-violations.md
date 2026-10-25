# Lint and suppression status

The workspace is currently clean against the Phase 8 policy:

- `cargo fmt --all -- --check` reports no formatting changes.
- `cargo clippy --workspace --all-targets -- -D warnings` reports no errors.
- The workspace carries **15** `#[allow]` attributes.

The `allow-listing` CI job records the current suppression count in
`docs/quality/lint-baseline.txt`. It fails only if the count grows above
that baseline. Most suppressions are concentrated in the AmigaOS ABI and
DOS structures, where CamelCase field names are part of the compatibility
surface.

To reproduce:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
grep -rn --include='*.rs' -E '#!?\[allow\(' . | grep -v '^\./target/'
```
