//! A window showing one remote screen and forwarding keyboard and pointer input to it.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme, Sizable as _, StyledExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use image::{Frame, RgbaImage};
use open_desk_media::DecodedFrame;
use open_desk_proto::{
    Availability, HostStatus, InputEvent, KeyCode, MouseButton as RemoteButton, PointerPosition,
};
use open_desk_session::{SessionEndReason, ViewerEvent, ViewerHandle};
use tokio::sync::mpsc;

use crate::keymap::{key_code, modifier_changes};
use crate::text::text;
use crate::video_layout::{ScrollAccumulator, letterbox, pointer_position};

const WINDOW_SIZE: Size<Pixels> = Size {
    width: px(1280.),
    height: px(800.),
};
const TOOLBAR_HEIGHT: Pixels = px(44.);
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// Key context of the remote screen while it has focus.
const REMOTE_SCREEN_CONTEXT: &str = "RemoteScreen";

/// Registers the viewer's key bindings. Call once during app initialization.
///
/// gpui-kit's `Root` binds Tab, Shift-Tab, and the platform copy shortcut for focus navigation
/// and copying. Inside the remote screen those keys belong to the remote machine, so they are
/// unbound there and reach the screen's key listeners instead.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", NoAction, Some(REMOTE_SCREEN_CONTEXT)),
        KeyBinding::new("shift-tab", NoAction, Some(REMOTE_SCREEN_CONTEXT)),
        KeyBinding::new("cmd-c", NoAction, Some(REMOTE_SCREEN_CONTEXT)),
        KeyBinding::new("ctrl-c", NoAction, Some(REMOTE_SCREEN_CONTEXT)),
    ]);
}

/// Opens a viewer window for an established session.
pub fn open_viewer_window(
    viewer: ViewerHandle,
    events: mpsc::UnboundedReceiver<ViewerEvent>,
    cx: &mut App,
) -> anyhow::Result<(AnyWindowHandle, Entity<ViewerView>)> {
    let title = format!("{} — open-desk", viewer.peer().name);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(WINDOW_SIZE, cx)),
        titlebar: Some(TitlebarOptions {
            title: Some(title.into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| ViewerView::new(viewer, events, window, cx))
    })
}

pub struct ViewerView {
    /// `None` once the session is over.
    session: Option<ViewerHandle>,
    peer_name: SharedString,
    image: Option<Arc<RenderImage>>,
    frame_size: Size<u32>,
    frames_shown: u64,
    /// Where the picture was last painted, for mapping the pointer.
    picture: Rc<Cell<Option<Bounds<Pixels>>>>,
    status: Option<HostStatus>,
    ended: Option<SessionEndReason>,
    focus: FocusHandle,
    modifiers: Modifiers,
    held_keys: Vec<KeyCode>,
    held_buttons: Vec<RemoteButton>,
    scroll: ScrollAccumulator,
    fps: f64,
    rtt: Duration,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for ViewerView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewerView")
            .field("peer", &self.peer_name)
            .finish_non_exhaustive()
    }
}

impl ViewerView {
    fn new(
        session: ViewerHandle,
        mut events: mpsc::UnboundedReceiver<ViewerEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);

        let mut frames = session.frames();
        let frame_task = cx.spawn_in(window, async move |this, cx| {
            while frames.changed().await.is_ok() {
                let frame = frames.borrow_and_update().clone();
                let Some(frame) = frame else { continue };
                let Ok(()) =
                    this.update_in(cx, |this, window, cx| this.show_frame(&frame, window, cx))
                else {
                    break;
                };
            }
        });
        let event_task = cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                let Ok(()) = this.update(cx, |this, cx| this.on_event(event, cx)) else {
                    break;
                };
            }
        });
        let stats_task = cx.spawn(async move |this, cx| {
            let mut last = (0u64, Instant::now());
            loop {
                cx.background_executor().timer(STATS_INTERVAL).await;
                let Ok(()) = this.update(cx, |this, cx| {
                    let Some(session) = &this.session else { return };
                    let frames = session.stats().frames_decoded.load(Ordering::Relaxed);
                    let elapsed = last.1.elapsed().as_secs_f64().max(0.001);
                    #[expect(clippy::cast_precision_loss, reason = "display only")]
                    let fps = (frames - last.0) as f64 / elapsed;
                    this.fps = fps;
                    this.rtt = session.rtt();
                    last = (frames, Instant::now());
                    cx.notify();
                }) else {
                    break;
                };
            }
        });
        // Keys held while the window loses focus would stay down on the host.
        let activation = cx.observe_window_activation(window, |this, window, _| {
            if !window.is_window_active() {
                this.release_all();
            }
        });

        Self {
            peer_name: session.peer().name.clone().into(),
            session: Some(session),
            image: None,
            frame_size: size(0, 0),
            frames_shown: 0,
            picture: Rc::new(Cell::new(None)),
            status: None,
            ended: None,
            focus,
            modifiers: Modifiers::default(),
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
            scroll: ScrollAccumulator::default(),
            fps: 0.,
            rtt: Duration::ZERO,
            _tasks: vec![frame_task, event_task, stats_task],
            _subscriptions: vec![activation],
        }
    }

    fn show_frame(&mut self, frame: &DecodedFrame, window: &mut Window, cx: &mut Context<Self>) {
        let Some(buffer) = RgbaImage::from_raw(frame.width, frame.height, frame.bgra.clone())
        else {
            return;
        };
        // `RenderImage` expects BGRA, which is what the decoder produces.
        let image = Arc::new(RenderImage::new(vec![Frame::new(buffer)]));
        if let Some(previous) = self.image.replace(image) {
            // Each frame is a new GPU texture; release the old one or the atlas grows forever.
            let _dropped = window.drop_image(previous);
        }
        self.frame_size = size(frame.width, frame.height);
        self.frames_shown += 1;
        cx.notify();
    }

    #[doc(hidden)]
    pub fn has_frame(&self) -> bool {
        self.image.is_some()
    }

    #[doc(hidden)]
    pub fn frames_shown(&self) -> u64 {
        self.frames_shown
    }

    fn on_event(&mut self, event: ViewerEvent, cx: &mut Context<Self>) {
        match event {
            ViewerEvent::HostStatus(status) => self.status = Some(status),
            ViewerEvent::Ended(reason) => {
                self.ended = Some(reason);
                self.session = None;
            }
        }
        cx.notify();
    }

    fn send(&mut self, event: InputEvent) {
        if let Some(session) = &self.session {
            session.send_input(event);
        }
    }

    fn release_all(&mut self) {
        for key in std::mem::take(&mut self.held_keys) {
            self.send(InputEvent::Key {
                key,
                pressed: false,
            });
        }
        for event in modifier_changes(std::mem::take(&mut self.modifiers), Modifiers::default()) {
            self.send(event);
        }
        for button in std::mem::take(&mut self.held_buttons) {
            self.send(InputEvent::PointerButton {
                button,
                pressed: false,
            });
        }
    }

    fn position(&self, position: Point<Pixels>) -> Option<PointerPosition> {
        pointer_position(self.picture.get()?, position)
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent) {
        if let Some(position) = self.position(event.position) {
            self.send(InputEvent::PointerMove(position));
        }
    }

    fn on_mouse_button(&mut self, button: MouseButton, position: Point<Pixels>, pressed: bool) {
        let button = remote_button(button);
        if pressed {
            // Only presses on the picture count; releases always go through so nothing sticks.
            let Some(position) = self.position(position) else {
                return;
            };
            self.send(InputEvent::PointerMove(position));
            if !self.held_buttons.contains(&button) {
                self.held_buttons.push(button);
            }
        } else if let Some(index) = self.held_buttons.iter().position(|held| *held == button) {
            self.held_buttons.remove(index);
        } else {
            return;
        }
        self.send(InputEvent::PointerButton { button, pressed });
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, window: &Window) {
        if self.position(event.position).is_none() {
            return;
        }
        let delta = event.delta.pixel_delta(window.line_height());
        if let Some((dx, dy)) = self.scroll.add(delta) {
            self.send(InputEvent::Scroll { dx, dy });
        }
    }

    fn on_modifiers(&mut self, modifiers: Modifiers) {
        for event in modifier_changes(self.modifiers, modifiers) {
            self.send(event);
        }
        self.modifiers = modifiers;
    }

    fn on_key(&mut self, keystroke: &Keystroke, pressed: bool) {
        // Keep the modifier state exact even if a modifier event was missed.
        self.on_modifiers(keystroke.modifiers);
        let Some(key) = key_code(keystroke) else {
            return;
        };
        if pressed {
            if !self.held_keys.contains(&key) {
                self.held_keys.push(key);
            }
        } else {
            self.held_keys.retain(|held| *held != key);
        }
        self.send(InputEvent::Key { key, pressed });
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.release_all();
        if let Some(session) = &self.session {
            session.disconnect();
        }
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let stats = if self.session.is_some() {
            format!("{:.0} fps · {} ms", self.fps, self.rtt.as_millis())
        } else {
            String::new()
        };
        div()
            .h_flex()
            .h(TOOLBAR_HEIGHT)
            .flex_none()
            .px_3()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child(self.peer_name.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(stats),
            )
            .child(div().flex_1())
            .when(self.session.is_some(), |toolbar| {
                toolbar.child(
                    Button::new("disconnect")
                        .small()
                        .danger()
                        .label(text().disconnect)
                        .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx))),
                )
            })
    }

    fn notice(&self) -> Option<String> {
        if let Some(reason) = &self.ended {
            return Some(format!(
                "{} — {}",
                text().session_ended,
                text().session_end_reason(reason)
            ));
        }
        let status = self.status?;
        match status.screen {
            Availability::PermissionDenied => return Some(text().remote_screen_permission.into()),
            Availability::Unavailable => return Some(text().remote_screen_unavailable.into()),
            Availability::Available => {}
        }
        (status.input != Availability::Available).then(|| text().remote_input_unavailable.into())
    }
}

fn remote_button(button: MouseButton) -> RemoteButton {
    match button {
        MouseButton::Left => RemoteButton::Left,
        MouseButton::Right => RemoteButton::Right,
        MouseButton::Middle => RemoteButton::Middle,
        MouseButton::Navigate(NavigationDirection::Back) => RemoteButton::Back,
        MouseButton::Navigate(NavigationDirection::Forward) => RemoteButton::Forward,
    }
}

impl Render for ViewerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let image = self.image.clone();
        let frame_size = self.frame_size;
        let picture = self.picture.clone();
        let screen = canvas(
            move |bounds, _, _| {
                let rect = letterbox(bounds, frame_size);
                picture.set(Some(rect));
                rect
            },
            move |_, rect, window, _| {
                if let Some(image) = image {
                    let _painted =
                        window.paint_image(rect, rect, Corners::default(), image, 0, false);
                }
            },
        )
        .size_full();

        let mut surface = div()
            .id("remote-screen")
            .key_context(REMOTE_SCREEN_CONTEXT)
            .relative()
            .flex_1()
            .min_h_0()
            .bg(gpui_kit::black())
            .track_focus(&self.focus)
            .on_mouse_move(
                cx.listener(|this, event: &MouseMoveEvent, _, _| this.on_mouse_move(event)),
            )
            .on_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                this.focus.focus(window, cx);
                this.on_mouse_button(event.button, event.position, true);
            }))
            .capture_any_mouse_up(cx.listener(|this, event: &MouseUpEvent, _, _| {
                this.on_mouse_button(event.button, event.position, false);
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, _| {
                this.on_scroll(event, window);
            }))
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, _| {
                this.on_modifiers(event.modifiers);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.on_key(&event.keystroke, true);
                cx.stop_propagation();
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, cx| {
                this.on_key(&event.keystroke, false);
                cx.stop_propagation();
            }))
            .child(screen);

        if self.image.is_none() && self.ended.is_none() {
            surface = surface.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(gpui_kit::white())
                    .child(text().waiting_for_screen),
            );
        }
        if let Some(notice) = self.notice() {
            let ended = self.ended.is_some();
            surface = surface.child(
                div()
                    .absolute()
                    .bottom_4()
                    .left_4()
                    .right_4()
                    .v_flex()
                    .gap_2()
                    .p_3()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().background.opacity(0.92))
                    .text_sm()
                    .child(notice)
                    .when(ended, |notice| {
                        notice.child(
                            Button::new("close-viewer")
                                .small()
                                .label(text().close)
                                .on_click(|_, window, _| window.remove_window()),
                        )
                    }),
            );
        }

        div()
            .v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_toolbar(cx))
            .child(surface)
    }
}
