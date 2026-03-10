# wl-zenwindow

A Rust library that dims Wayland monitors using layer-shell overlays. Put dark, translucent surfaces over every monitor except the one you're working on — the Wayland equivalent of turning off the other screens.

## Features

Works with any compositor that supports `wlr-layer-shell`: Sway, Hyprland, Niri, River, Labwc, and others.

- Tracks the focused window and keeps its monitor undimmed
- Cross-fades overlays when focus moves between monitors
- Optionally dims monitor brightness via gamma control (falls back if another client like wlsunset already has it)
- Configurable overlay color, opacity, fade duration, and settle delay
- Dropping the handle removes overlays and restores gamma

## Requirements

- `libxkbcommon-dev` (or your distro's equivalent)
- A Wayland compositor with `wlr-layer-shell-unstable-v1` support
- `zwlr_foreign_toplevel_manager_v1` for focus tracking (optional, falls back to dimming all outputs)
- `zwlr_gamma_control_v1` for brightness dimming (optional)

## Installation

Add `wl-zenwindow` to your `Cargo.toml`:

```toml
[dependencies]
wl-zenwindow = "0.1"
```

See the [API documentation](https://docs.rs/wl-zenwindow) for the full builder API and configuration options.

## Getting started

The simplest thing you can do is put a black overlay on every monitor:

```rust
use wl_zenwindow::ZenWindow;

fn main() {
    let _zen = ZenWindow::builder()
        .spawn()
        .expect("failed to start overlays");

    // Overlays stay up as long as `_zen` is alive.
    // Park the thread so the program doesn't exit immediately.
    std::thread::park();
}
```

`spawn()` connects to Wayland, creates overlay surfaces on each output, and returns a handle. The overlays live as long as the handle does — drop it and they disappear.

Dimming *all* monitors isn't very useful though. Add `skip_active()` to leave the monitor with the focused window clear, and `opacity()` to make the overlays translucent so you can still see what's behind them:

```rust
let _zen = ZenWindow::builder()
    .skip_active()
    .opacity(0.85)
    .spawn()
    .expect("failed to start overlays");
```

Now when you move focus to a different monitor, the overlay follows — the old monitor dims and the new one clears.

Overlays only darken the visual — the backlight stays at full brightness. If your compositor supports `zwlr_gamma_control_v1`, add `brightness()` to dim the actual monitor brightness too. And `fade_in()` keeps the overlays from snapping on instantly:

```rust
use std::time::Duration;

let _zen = ZenWindow::builder()
    .skip_active()
    .opacity(0.85)
    .brightness(0.7)
    .fade_in(Duration::from_millis(500))
    .spawn()
    .expect("failed to start overlays");
```

If another client already controls gamma (like `wlsunset` or `gammastep`), the brightness setting is silently skipped rather than fighting over it. The fade uses an ease-out curve, so it starts fast and decelerates.

When you're done, drop the handle and everything is restored — overlays are removed and gamma goes back to normal:

```rust
drop(zen);
```

Or just let it go out of scope. There's no explicit cleanup API to call.

## Usage

### Dimming only specific monitors

If you want to always dim certain monitors regardless of focus, use `skip_output()` to exclude them by their Wayland name:

```rust
// Only dim DP-2 and HDMI-1; leave DP-1 and eDP-1 alone
let _zen = ZenWindow::builder()
    .skip_output("DP-1")
    .skip_output("eDP-1")
    .spawn()
    .expect("failed to start overlays");
```

You can find your output names with `swaymsg -t get_outputs`, `hyprctl monitors`, or `wlr-randr`.

Combine `skip_output()` with `skip_active()` to dim everything except specific monitors *and* the focused one:

```rust
let _zen = ZenWindow::builder()
    .skip_output("eDP-1") // never dim the laptop screen
    .skip_active()         // also skip whichever monitor has focus
    .spawn()
    .expect("failed to start overlays");
```

### Handling errors

`spawn()` returns a `Result<ZenWindow, SpawnError>`. If you want to fall back gracefully instead of crashing:

```rust
use wl_zenwindow::{ZenWindow, SpawnError};

let zen = match ZenWindow::builder().skip_active().spawn() {
    Ok(handle) => Some(handle),
    Err(SpawnError::WaylandConnection(_)) => {
        eprintln!("not running on Wayland, skipping overlays");
        None
    }
    Err(SpawnError::MissingProtocol { protocol, .. }) => {
        eprintln!("compositor missing {protocol}, skipping overlays");
        None
    }
    Err(e) => {
        eprintln!("overlay setup failed: {e}");
        None
    }
};
```

If you don't care about errors at all, `spawn_nonblocking()` returns a handle directly and fails silently:

```rust
let _zen = ZenWindow::builder()
    .skip_active()
    .spawn_nonblocking();
```

### Integrating with a UI framework

When launching overlays alongside a UI window, the window needs time to appear and gain focus before the library detects the active output. Use `settle_delay()` to wait:

```rust
use std::time::Duration;

let _zen = ZenWindow::builder()
    .skip_active()
    .settle_delay(Duration::from_millis(200))
    .fade_in(Duration::from_millis(500))
    .spawn_nonblocking();
```

Use `spawn_nonblocking()` so the overlays don't block your UI's event loop. The settle delay runs on the background thread, not the calling thread.

See [`examples/gpui/main.rs`](examples/gpui/main.rs) for a complete example using [GPUI](https://gpui.rs).

### Tuning timing to avoid flicker

Two settings control timing:

- **`settle_delay`** — how long to wait after startup before creating surfaces. Use this when launching alongside a window that needs time to render and gain focus.
- **`fade_in`** — how long the overlays take to fade from transparent to their target opacity.

If you see the wrong monitor dim briefly on startup, increase the settle delay. If the overlays feel too abrupt, increase the fade duration. If focus changes cause flickering, the library handles that internally with a separate cross-fade transition.

```rust
use std::time::Duration;

let _zen = ZenWindow::builder()
    .skip_active()
    .settle_delay(Duration::from_millis(300)) // wait for window to settle
    .fade_in(Duration::from_millis(400))      // smooth fade-in
    .spawn()
    .expect("failed to start overlays");
```

## Examples

The [`examples/gpui`](examples/gpui/main.rs) example opens a fullscreen window using [GPUI](https://gpui.rs) and dims all other monitors:

```sh
cargo run --example gpui
```

## License

wl-zenwindow is licensed under the MIT license. See the [`LICENSE`](LICENSE) file for more information.
