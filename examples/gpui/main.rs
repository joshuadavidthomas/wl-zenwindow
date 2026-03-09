use gpui::*;

actions!(gpui_example, [Quit]);

struct ZenView;

impl Render for ZenView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x1c1c1c))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_color(rgb(0xd4d4d4))
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
            |_window, cx| cx.new(|_cx| ZenView),
        )
        .unwrap();

        cx.activate(true);

        let _zen = wl_zenwindow::ZenWindow::builder()
            .namespace("wl-zenwindow-demo")
            .skip_active()
            .settle_delay(std::time::Duration::from_millis(200))
            .fade_in(std::time::Duration::from_millis(500))
            .spawn_nonblocking();
    });
}
