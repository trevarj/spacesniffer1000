# Project Rules

## Rust

- Keep modules small and behavior-focused.
- Prefer explicit error handling over `unwrap` in app flow.
- Use `expect` only for invariants proven by nearby code.
- Keep filesystem mutation paths isolated and testable.
- Add short comments for non-obvious traversal, safety, or UI state logic.
- Avoid blocking work in `eframe::App::update`; background scan events must be drained incrementally.

## GUI

- Keep the first screen usable: path entry, mounted filesystem shortcuts, scan controls, treemap, and inspector.
- Text must fit compact panels and treemap labels must disappear before they overlap.
- Destructive actions require confirmation. Permanent delete requires typing the exact path.

## Guix

- Do not use global package installs.
- Use `guix shell -m manifest.scm -- <command>` for development commands.
- `guix.scm` is the package entry point. If Cargo dependencies change, update the package inputs/vendoring notes.

## Verification

- `guix shell -m manifest.scm -- cargo test`
- `guix shell -m manifest.scm -- cargo check`
- `guix shell -m manifest.scm -- cargo clippy --all-targets -- -D warnings`

Run the app with:

```sh
guix shell -m manifest.scm -- cargo run
```
