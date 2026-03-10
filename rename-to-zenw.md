# Name the CLI binary `zenw`

## Summary

`wl-zenwindow` is a mouthful to type as a CLI command. The binary should be named `zenw` — short, memorable, and clearly traces back to the project name. The crate/library name stays `wl-zenwindow`.

## Changes Required

### Cargo.toml

Add a `[[bin]]` section so the compiled binary is named `zenw`:

```toml
[[bin]]
name = "zenw"
path = "src/main.rs"
```

### src/main.rs

Create the CLI entry point. This doesn't exist yet — needs to be written as part of turning the library into a binary (or a library + binary crate).

### README.md

- Update usage/install instructions to reference the `zenw` command
- Add CLI usage examples

## Considerations

- The crate name `wl-zenwindow` and all library imports (`use wl_zenwindow::...`) stay the same — no breaking change for library consumers
- `cargo install wl-zenwindow` would produce a `zenw` binary — worth calling out in docs
- Shell completions, man pages, etc. should use `zenw` when added later
