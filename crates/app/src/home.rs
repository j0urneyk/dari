//! Home window: entry point for sharing this device and connecting to a remote one.

use gpui_kit::component::*;
use gpui_kit::*;

#[derive(Debug)]
pub(crate) struct Home;

impl Home {
    pub(crate) fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self
    }
}

impl Render for Home {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(div().text_xl().child("open-desk"))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Remote desktop for macOS and Windows"),
            )
    }
}
