# Experiments

This directory contains experiments, work-in-progress crates, or other code and documentation, that
do not fall under the same stability guarantees as the main crates (`matrix-sdk`,
`matrix-sdk-crypto`, etc.).

Lab projects might be abandoned and possibly removed at any time.

---

That said, this directory is meant to freely explore unconventional or interesting ways the Matrix
Rust SDK can evolve, feel free to propose an experiment.

## Current experiments

- multiverse: a TUI client mostly for quick development iteration of SDK features and debugging.
  - Run with `cargo run --bin multiverse matrix.org ~/.cache/multiverse-cache`.
  - To inspect network requests, there is a `--proxy` option which can use in
    combination with [mitmproxy](https://www.mitmproxy.org/):
    `cargo run --bin multiverse -- --proxy http://localhost:8080 matrix.org ~/.cache/multiverse-cache`

## Archived experiments

_Link to PR that deleted the experiment from the repo, newest first_:
