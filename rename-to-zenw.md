# Rename crate/CLI from `wl-zenwindow` to `zenw`

## Summary

`wl-zenwindow` is a mouthful as a command/crate name. Rename to `zenw` — short, memorable, and clearly traces back to the project.

## Changes Required

### Cargo.toml

- Package `name` field: `wl-zenwindow` → `zenw`

### release-plz.toml

- Package `name`: `wl-zenwindow` → `zenw`

### src/window.rs (6 references)

- Default namespace: `"wl-zenwindow"` → `"zenw"`
- Doc comment for `namespace()` referencing the default
- Thread names in `spawn()` and `spawn_nonblocking()`
- Two test assertions checking the default namespace

### src/lib.rs

- Documentation table showing default namespace value

### src/render.rs

- Memfd identifier: `"wl-zenwindow-gamma"` → `"zenw-gamma"`

### examples/gpui/main.rs

- Example namespace: `"wl-zenwindow-demo"` → `"zenw-demo"`

### README.md

- Title, dependency instructions, `use` imports (`wl_zenwindow::` → `zenw::`), docs.rs link, license text

### Cargo.lock

- Auto-updates from Cargo.toml change

## Considerations

- **Downstream imports** change from `use wl_zenwindow::...` to `use zenw::...` — this is a breaking change
- **crates.io** — if published, this would be a new crate name (could yank old, point to new)
- **Repository URL** in Cargo.toml may need updating if the repo is renamed
- No CI, shell completions, man pages, or service files need changes
