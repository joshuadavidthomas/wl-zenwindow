//! Example: fullscreen GPUI window with dimmed monitors.
#![allow(missing_docs)] // GPUI macros generate undocumented items

use gpui::actions;
use gpui::div;
use gpui::point;
use gpui::px;
use gpui::rgb;
use gpui::size;
use gpui::App;
use gpui::AppContext;
use gpui::Application;
use gpui::Bounds;
use gpui::Context;
use gpui::IntoElement;
use gpui::KeyBinding;
use gpui::ParentElement;
use gpui::Render;
use gpui::Styled;
use gpui::Window;
use gpui::WindowBounds;
use gpui::WindowOptions;

actions!(gpui_example, [Quit]);

struct ZenView {
    // Keep the zen handle alive for the lifetime of the view
    _zen: wl_zenwindow::ZenWindow,
}

impl Render for ZenView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x001C_1C1C))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_color(rgb(0x00D4_D4D4))
                    .text_size(px(18.))
                    .child("Press Escape to quit"),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("escape", Quit, None),
            KeyBinding::new("ctrl-q", Quit, None),
        ]);

        cx.on_action(|_: &Quit, cx| {
            cx.quit();
        });

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Fullscreen(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(1920.), px(1080.)),
                })),
                titlebar: None,
                focus: true,
                show: true,
                ..Default::default()
            },
            |_window, cx| {
                let zen = wl_zenwindow::ZenWindow::builder()
                    .namespace("wl-zenwindow-demo")
                    .skip_active()
                    .settle_delay(std::time::Duration::from_millis(200))
                    .fade_in(std::time::Duration::from_millis(500))
                    .spawn_nonblocking();

                cx.new(|_cx| ZenView { _zen: zen })
            },
        )
        .unwrap();

        cx.activate(true);
    });
}
