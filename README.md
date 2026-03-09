# wl-zenwindow

Add zen mode to your Wayland app in a few lines. Dims all monitors except the one you're focused on, with smooth cross-fade transitions as you move between outputs.

## Features

Works with any compositor that supports `wlr-layer-shell`: Sway, Hyprland, Niri, River, Labwc, and others.

- Tracks the focused window and keeps its monitor undimmed
- Cross-fades overlays when focus moves between monitors
- Optionally dims monitor brightness via gamma control (falls back if another client like wlsunset already has it)
- Configurable overlay color, opacity, fade duration, and settle delay
- Dropping the handle removes overlays and restores gamma

## Requirements

- A Wayland compositor with `wlr-layer-shell-unstable-v1` support
- `zwlr_foreign_toplevel_manager_v1` for focus tracking (optional, falls back to dimming all outputs)
- `zwlr_gamma_control_v1` for brightness dimming (optional)

See the [API documentation](https://docs.rs/wl-zenwindow) for the full builder API and configuration options.

## Usage

We'll set up zen overlays that dim every monitor except the one with your focused window, then clean up when we're done.

```rust
use wl_zenwindow::ZenWindow;
use std::time::Duration;
```

Create a builder, tell it to skip the active monitor, and add a fade-in so the overlays don't just snap on:

```rust
let zen = ZenWindow::builder()
    .skip_active()
    .opacity(0.85)
    .brightness(0.7)
    .fade_in(Duration::from_millis(500))
    .spawn()?;
```

`spawn()` connects to Wayland, creates overlay surfaces on each output, and fades them in on a background thread. It returns a handle — the overlays stay up as long as the handle is alive.

When the user moves focus to a different monitor, the overlay on that monitor fades out and the previous monitor fades back in.

When you're done, drop the handle. The overlays are removed and gamma is restored:

```rust
drop(zen);
```

## Examples

The [`examples/gpui`](examples/gpui/main.rs) example opens a fullscreen window using [GPUI](https://gpui.rs) and dims all other monitors:

```sh
cargo run --example gpui
```

## License

wl-zenwindow is licensed under the MIT license. See the [`LICENSE`](LICENSE) file for more information.
