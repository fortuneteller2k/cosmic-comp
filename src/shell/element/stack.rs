use super::CosmicSurface;
use crate::{
    shell::{focus::FocusDirection, layout::tiling::Direction, Shell},
    state::State,
    utils::iced::{IcedElement, Program},
    utils::prelude::SeatExt,
    wayland::handlers::screencopy::ScreencopySessions,
};
use apply::Apply;
use calloop::LoopHandle;
use cosmic::{
    iced::{id::Id, widget as iced_widget},
    iced_core::{Background, BorderRadius, Color, Length},
    iced_runtime::Command,
    iced_widget::scrollable::AbsoluteOffset,
    theme, widget as cosmic_widget, Element as CosmicElement,
};
use cosmic_protocols::screencopy::v1::server::zcosmic_screencopy_session_v1::InputType;
use once_cell::sync::Lazy;
use smithay::{
    backend::{
        input::KeyState,
        renderer::{
            element::{
                memory::MemoryRenderBufferRenderElement, surface::WaylandSurfaceRenderElement,
                AsRenderElements,
            },
            ImportAll, ImportMem, Renderer,
        },
    },
    desktop::space::SpaceElement,
    input::{
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerTarget, RelativeMotionEvent},
        Seat,
    },
    output::Output,
    render_elements,
    utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale, Serial, Size},
    wayland::seat::WaylandFocus,
};
use std::{
    fmt,
    hash::Hash,
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

mod tab;
mod tab_text;
mod tabs;

use self::{
    tab::{Tab, TabMessage},
    tabs::Tabs,
};

static SCROLLABLE_ID: Lazy<Id> = Lazy::new(|| Id::new("scrollable"));

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CosmicStack(IcedElement<CosmicStackInternal>);

impl fmt::Debug for CosmicStack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CosmicStack")
            .field("internal", &self.0)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct CosmicStackInternal {
    windows: Arc<Mutex<Vec<CosmicSurface>>>,
    active: Arc<AtomicUsize>,
    activated: Arc<AtomicBool>,
    group_focused: Arc<AtomicBool>,
    scroll_to_focus: Arc<AtomicBool>,
    previous_keyboard: Arc<AtomicUsize>,
    pointer_entered: Arc<AtomicU8>,
    previous_pointer: Arc<AtomicUsize>,
    last_seat: Arc<Mutex<Option<(Seat<State>, Serial)>>>,
    last_location: Arc<Mutex<Option<(Point<f64, Logical>, Serial, u32)>>>,
    geometry: Arc<Mutex<Option<Rectangle<i32, Logical>>>>,
    mask: Arc<Mutex<Option<tiny_skia::Mask>>>,
}

impl CosmicStackInternal {
    pub fn swap_focus(&self, focus: Focus) -> Focus {
        unsafe {
            std::mem::transmute::<u8, Focus>(
                self.pointer_entered.swap(focus as u8, Ordering::SeqCst),
            )
        }
    }

    pub fn current_focus(&self) -> Focus {
        unsafe { std::mem::transmute::<u8, Focus>(self.pointer_entered.load(Ordering::SeqCst)) }
    }

    //pub fn offsets_for_tabs(&self) -> Vec<
}

const TAB_HEIGHT: i32 = 24;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Focus {
    None,
    Header,
    Window,
}

pub enum MoveResult {
    Handled,
    MoveOut(CosmicSurface, LoopHandle<'static, crate::state::Data>),
    Default,
}

impl CosmicStack {
    pub fn new<I: Into<CosmicSurface>>(
        windows: impl Iterator<Item = I>,
        handle: LoopHandle<'static, crate::state::Data>,
    ) -> CosmicStack {
        let windows = windows.map(Into::into).collect::<Vec<_>>();
        assert!(!windows.is_empty());

        for window in &windows {
            window.try_force_undecorated(true);
            window.set_tiled(true);
        }

        let width = windows[0].geometry().size.w;
        CosmicStack(IcedElement::new(
            CosmicStackInternal {
                windows: Arc::new(Mutex::new(windows)),
                active: Arc::new(AtomicUsize::new(0)),
                activated: Arc::new(AtomicBool::new(false)),
                group_focused: Arc::new(AtomicBool::new(false)),
                scroll_to_focus: Arc::new(AtomicBool::new(false)),
                previous_keyboard: Arc::new(AtomicUsize::new(0)),
                pointer_entered: Arc::new(AtomicU8::new(Focus::None as u8)),
                previous_pointer: Arc::new(AtomicUsize::new(0)),
                last_seat: Arc::new(Mutex::new(None)),
                last_location: Arc::new(Mutex::new(None)),
                geometry: Arc::new(Mutex::new(None)),
                mask: Arc::new(Mutex::new(None)),
            },
            (width, TAB_HEIGHT),
            handle,
        ))
    }

    pub fn add_window(&self, window: impl Into<CosmicSurface>, idx: Option<usize>) {
        let window = window.into();
        window.try_force_undecorated(true);
        window.set_tiled(true);
        self.0.with_program(|p| {
            if let Some(mut geo) = p.geometry.lock().unwrap().clone() {
                geo.loc.y += TAB_HEIGHT;
                geo.size.h -= TAB_HEIGHT;
                window.set_geometry(geo);
            }
            window.send_configure();
            if let Some(idx) = idx {
                p.windows.lock().unwrap().insert(idx, window);
                p.active.store(idx, Ordering::SeqCst);
            } else {
                let mut windows = p.windows.lock().unwrap();
                windows.push(window);
                p.active.store(windows.len() - 1, Ordering::SeqCst);
            }
            p.scroll_to_focus.store(true, Ordering::SeqCst);
        });
        self.0.force_redraw()
    }

    pub fn remove_window(&self, window: &CosmicSurface) {
        self.0.with_program(|p| {
            let mut windows = p.windows.lock().unwrap();
            if windows.len() == 1 {
                return;
            }

            let Some(idx) = windows.iter().position(|w| w == window) else { return };
            let window = windows.remove(idx);
            window.try_force_undecorated(false);
            window.set_tiled(false);

            p.active.fetch_min(windows.len() - 1, Ordering::SeqCst);
        });
        self.0.force_redraw()
    }

    pub fn remove_idx(&self, idx: usize) {
        self.0.with_program(|p| {
            let mut windows = p.windows.lock().unwrap();
            if windows.len() == 1 {
                return;
            }
            if windows.len() >= idx {
                return;
            }
            let window = windows.remove(idx);
            window.try_force_undecorated(false);
            window.set_tiled(false);

            p.active.fetch_min(windows.len() - 1, Ordering::SeqCst);
        });
        self.0.force_redraw()
    }

    pub fn len(&self) -> usize {
        self.0.with_program(|p| p.windows.lock().unwrap().len())
    }

    pub fn handle_focus(&self, direction: FocusDirection) -> bool {
        let result = self.0.with_program(|p| match direction {
            FocusDirection::Left => {
                if !p.group_focused.load(Ordering::SeqCst) {
                    if let Ok(old) =
                        p.active
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |val| {
                                val.checked_sub(1)
                            })
                    {
                        p.previous_keyboard.store(old, Ordering::SeqCst);
                        p.previous_pointer.store(old, Ordering::SeqCst);
                        p.scroll_to_focus.store(true, Ordering::SeqCst);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            FocusDirection::Right => {
                if !p.group_focused.load(Ordering::SeqCst) {
                    let max = p.windows.lock().unwrap().len();
                    if let Ok(old) =
                        p.active
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |val| {
                                if val < max - 1 {
                                    Some(val + 1)
                                } else {
                                    None
                                }
                            })
                    {
                        p.previous_keyboard.store(old, Ordering::SeqCst);
                        p.previous_pointer.store(old, Ordering::SeqCst);
                        p.scroll_to_focus.store(true, Ordering::SeqCst);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            FocusDirection::Out => {
                if !p.group_focused.swap(true, Ordering::SeqCst) {
                    p.windows.lock().unwrap().iter().for_each(|w| {
                        w.set_activated(false);
                        w.send_configure();
                    });
                    true
                } else {
                    false
                }
            }
            FocusDirection::In => {
                if !p.group_focused.swap(false, Ordering::SeqCst) {
                    p.windows.lock().unwrap().iter().for_each(|w| {
                        w.set_activated(true);
                        w.send_configure();
                    });
                    true
                } else {
                    false
                }
            }
            _ => false,
        });

        if result {
            self.0.force_update();
        }

        result
    }

    pub fn handle_move(&self, direction: Direction) -> MoveResult {
        let loop_handle = self.0.loop_handle();
        self.0.with_program(|p| {
            if p.group_focused.load(Ordering::SeqCst) {
                return MoveResult::Default;
            }

            let active = p.active.load(Ordering::SeqCst);
            let mut windows = p.windows.lock().unwrap();

            let next = match direction {
                Direction::Left => active.checked_sub(1),
                Direction::Right => (active + 1 < windows.len()).then_some(active + 1),
                Direction::Down | Direction::Up => None,
            };

            if let Some(val) = next {
                let old = p.active.swap(val, Ordering::SeqCst);
                windows.swap(old, val);
                p.previous_keyboard.store(old, Ordering::SeqCst);
                p.previous_pointer.store(old, Ordering::SeqCst);
                p.scroll_to_focus.store(true, Ordering::SeqCst);
                MoveResult::Handled
            } else {
                if windows.len() == 1 {
                    return MoveResult::Default;
                }
                let window = windows.remove(active);
                if active == windows.len() {
                    p.active.store(active - 1, Ordering::SeqCst);
                    p.scroll_to_focus.store(true, Ordering::SeqCst);
                }
                window.try_force_undecorated(false);
                window.set_tiled(false);

                MoveResult::MoveOut(window, loop_handle)
            }
        })
    }

    pub fn active(&self) -> CosmicSurface {
        self.0
            .with_program(|p| p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)].clone())
    }

    pub fn has_active(&self, window: &CosmicSurface) -> bool {
        self.0
            .with_program(|p| &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)] == window)
    }

    pub fn set_active(&self, window: &CosmicSurface) {
        self.0.with_program(|p| {
            if let Some(val) = p.windows.lock().unwrap().iter().position(|w| w == window) {
                let old = p.active.swap(val, Ordering::SeqCst);
                p.previous_keyboard.store(old, Ordering::SeqCst);
                p.previous_pointer.store(old, Ordering::SeqCst);
            }
        });
        self.0.force_redraw()
    }

    pub fn surfaces(&self) -> impl Iterator<Item = CosmicSurface> {
        self.0.with_program(|p| {
            p.windows
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
        })
    }

    pub fn offset(&self) -> Point<i32, Logical> {
        Point::from((0, TAB_HEIGHT))
    }

    pub fn set_geometry(&self, geo: Rectangle<i32, Logical>) {
        self.0.with_program(|p| {
            let loc = (geo.loc.x, geo.loc.y + TAB_HEIGHT);
            let size = (geo.size.w, geo.size.h - TAB_HEIGHT);

            let win_geo = Rectangle::from_loc_and_size(loc, size);
            for window in p.windows.lock().unwrap().iter() {
                window.set_geometry(win_geo);
            }

            *p.geometry.lock().unwrap() = Some(geo);
            p.mask.lock().unwrap().take();
        });
        self.0.resize(Size::from((geo.size.w, TAB_HEIGHT)));
    }

    fn keyboard_leave_if_previous(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        serial: Serial,
    ) -> usize {
        self.0.with_program(|p| {
            let active = p.active.load(Ordering::SeqCst);
            let previous = p.previous_keyboard.swap(active, Ordering::SeqCst);
            if previous != active {
                let windows = p.windows.lock().unwrap();
                if let Some(previous) = windows.get(previous) {
                    KeyboardTarget::leave(previous, seat, data, serial);
                }
                KeyboardTarget::enter(
                    &windows[active],
                    seat,
                    data,
                    Vec::new(), /* TODO */
                    serial,
                )
            }
            active
        })
    }

    fn pointer_leave_if_previous(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        serial: Serial,
        time: u32,
        location: Point<f64, Logical>,
    ) -> usize {
        self.0.with_program(|p| {
            let active = p.active.load(Ordering::SeqCst);
            let previous = p.previous_pointer.swap(active, Ordering::SeqCst);
            if previous != active {
                let windows = p.windows.lock().unwrap();
                if let Some(previous) = windows.get(previous) {
                    if let Some(sessions) = previous.user_data().get::<ScreencopySessions>() {
                        for session in &*sessions.0.borrow() {
                            session.cursor_leave(seat, InputType::Pointer)
                        }
                    }
                    PointerTarget::leave(previous, seat, data, serial, time);
                }

                if let Some(sessions) = windows[active].user_data().get::<ScreencopySessions>() {
                    for session in &*sessions.0.borrow() {
                        session.cursor_enter(seat, InputType::Pointer)
                    }
                }
                PointerTarget::enter(
                    &windows[active],
                    seat,
                    data,
                    &MotionEvent {
                        location,
                        serial,
                        time,
                    },
                );
            }
            active
        })
    }

    pub(in super::super) fn focus_stack(&self) {
        self.0
            .with_program(|p| p.group_focused.store(true, Ordering::SeqCst));
    }

    pub(super) fn loop_handle(&self) -> LoopHandle<'static, crate::state::Data> {
        self.0.loop_handle()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Message {
    DragStart,
    Activate(usize),
    Close(usize),
    ScrollForward,
    ScrollBack,
    Scrolled,
}

impl TabMessage for Message {
    fn activate(idx: usize) -> Self {
        Message::Activate(idx)
    }

    fn is_activate(&self) -> Option<usize> {
        match self {
            Message::Activate(idx) => Some(*idx),
            _ => None,
        }
    }

    fn scroll_back() -> Self {
        Message::ScrollBack
    }

    fn scroll_further() -> Self {
        Message::ScrollForward
    }

    fn populate_scroll(&mut self, mut current_offset: AbsoluteOffset) -> Option<AbsoluteOffset> {
        match self {
            Message::ScrollBack => Some({
                current_offset.x -= 10.;
                current_offset
            }),
            Message::ScrollForward => Some({
                current_offset.x += 10.;
                current_offset
            }),
            _ => None,
        }
    }

    fn scrolled() -> Self {
        Message::Scrolled
    }
}

impl Program for CosmicStackInternal {
    type Message = Message;

    fn update(
        &mut self,
        message: Self::Message,
        loop_handle: &LoopHandle<'static, crate::state::Data>,
    ) -> Command<Self::Message> {
        match message {
            Message::DragStart => {
                if let Some((seat, serial)) = self.last_seat.lock().unwrap().clone() {
                    if let Some(surface) = self.windows.lock().unwrap()
                        [self.active.load(Ordering::SeqCst)]
                    .wl_surface()
                    {
                        loop_handle.insert_idle(move |data| {
                            Shell::move_request(&mut data.state, &surface, &seat, serial);
                        });
                    }
                }
            }
            Message::Activate(idx) => {
                if self.windows.lock().unwrap().get(idx).is_some() {
                    let old = self.active.swap(idx, Ordering::SeqCst);
                    self.previous_keyboard.store(old, Ordering::SeqCst);
                    self.previous_pointer.store(old, Ordering::SeqCst);
                    self.scroll_to_focus.store(true, Ordering::SeqCst);
                }
            }
            Message::Close(idx) => {
                if let Some(val) = self.windows.lock().unwrap().get(idx) {
                    val.close()
                }
            }
            Message::Scrolled => {
                self.scroll_to_focus.store(false, Ordering::SeqCst);
            }
            _ => unreachable!(),
        }
        Command::none()
    }

    fn view(&self) -> CosmicElement<'_, Self::Message> {
        let windows = self.windows.lock().unwrap();
        let Some(width) = self
            .geometry
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.size.w)
        else {
            return iced_widget::row(Vec::new()).into();
        };
        let active = self.active.load(Ordering::SeqCst);
        let group_focused = self.group_focused.load(Ordering::SeqCst);

        let elements = vec![
            cosmic_widget::icon("window-stack-symbolic", 16)
                .force_svg(true)
                .style(if group_focused {
                    theme::Svg::custom(|theme| iced_widget::svg::Appearance {
                        color: Some(if theme.cosmic().is_dark {
                            Color::BLACK
                        } else {
                            Color::WHITE
                        }),
                    })
                } else {
                    theme::Svg::Symbolic
                })
                .apply(iced_widget::container)
                .padding([4, 24])
                .center_y()
                .apply(iced_widget::mouse_area)
                .on_press(Message::DragStart)
                .into(),
            CosmicElement::new(
                Tabs::new(
                    windows.iter().enumerate().map(|(i, w)| {
                        let user_data = w.user_data();
                        user_data.insert_if_missing(Id::unique);
                        Tab::new(
                            w.title(),
                            w.app_id(),
                            user_data.get::<Id>().unwrap().clone(),
                        )
                        .on_close(Message::Close(i))
                    }),
                    active,
                    windows[active].is_activated(false),
                    group_focused,
                )
                .id(SCROLLABLE_ID.clone())
                .force_visible(
                    self.scroll_to_focus
                        .load(Ordering::SeqCst)
                        .then_some(active),
                )
                .height(Length::Fill)
                .width(Length::Fill),
            ),
            iced_widget::horizontal_space(64)
                .apply(iced_widget::mouse_area)
                .on_press(Message::DragStart)
                .into(),
        ];

        iced_widget::row(elements)
            .height(TAB_HEIGHT as u16)
            .width(width as u16)
            .apply(iced_widget::container)
            .center_y()
            .style(if self.group_focused.load(Ordering::SeqCst) {
                theme::Container::custom(|theme| iced_widget::container::Appearance {
                    text_color: Some(Color::from(theme.cosmic().background.on)),
                    background: Some(Background::Color(theme.cosmic().accent_color().into())),
                    border_radius: BorderRadius::from([8.0, 8.0, 0.0, 0.0]),
                    border_width: 0.0,
                    border_color: Color::TRANSPARENT,
                })
            } else {
                theme::Container::custom(|theme| iced_widget::container::Appearance {
                    text_color: Some(Color::from(theme.cosmic().background.on)),
                    background: Some(Background::Color(theme.cosmic().palette.neutral_3.into())),
                    border_radius: BorderRadius::from([8.0, 8.0, 0.0, 0.0]),
                    border_width: 0.0,
                    border_color: Color::TRANSPARENT,
                })
            })
            .into()
    }

    fn foreground(
        &self,
        pixels: &mut tiny_skia::PixmapMut<'_>,
        damage: &[Rectangle<i32, smithay::utils::Buffer>],
        scale: f32,
    ) {
        if self.group_focused.load(Ordering::SeqCst) {
            let border = Rectangle::from_loc_and_size(
                (0, TAB_HEIGHT - scale as i32),
                (pixels.width() as i32, scale as i32),
            );

            let mut paint = tiny_skia::Paint::default();
            let (b, g, r, a) = theme::COSMIC_DARK.accent_color().into_components();
            paint.set_color(tiny_skia::Color::from_rgba(r, g, b, a).unwrap());

            for rect in damage {
                if let Some(overlap) = rect.intersection(border) {
                    pixels.fill_rect(
                        tiny_skia::Rect::from_xywh(
                            overlap.loc.x as f32,
                            overlap.loc.y as f32,
                            overlap.size.w as f32,
                            overlap.size.h as f32,
                        )
                        .unwrap(),
                        &paint,
                        Default::default(),
                        None,
                    )
                }
            }
        }
    }
}

impl IsAlive for CosmicStack {
    fn alive(&self) -> bool {
        self.0
            .with_program(|p| p.windows.lock().unwrap().iter().any(IsAlive::alive))
    }
}

impl SpaceElement for CosmicStack {
    fn bbox(&self) -> Rectangle<i32, Logical> {
        self.0.with_program(|p| {
            let mut bbox =
                SpaceElement::bbox(&p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)]);
            bbox.size.h += TAB_HEIGHT;
            bbox
        })
    }
    fn is_in_input_region(&self, point: &Point<f64, Logical>) -> bool {
        let mut point = *point;
        if point.y < TAB_HEIGHT as f64 {
            return true;
        }
        point.y -= TAB_HEIGHT as f64;
        self.0.with_program(|p| {
            SpaceElement::is_in_input_region(
                &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)],
                &point,
            )
        })
    }
    fn set_activate(&self, activated: bool) {
        SpaceElement::set_activate(&self.0, activated);
        self.0.force_redraw();
        self.0.with_program(|p| {
            p.activated.store(activated, Ordering::SeqCst);
            if !p.group_focused.load(Ordering::SeqCst) {
                p.windows
                    .lock()
                    .unwrap()
                    .iter()
                    .for_each(|w| SpaceElement::set_activate(w, activated))
            }
        });
    }
    fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        SpaceElement::output_enter(&self.0, output, overlap);
        self.0.with_program(|p| {
            p.windows
                .lock()
                .unwrap()
                .iter()
                .for_each(|w| SpaceElement::output_enter(w, output, overlap))
        })
    }
    fn output_leave(&self, output: &Output) {
        SpaceElement::output_leave(&self.0, output);
        self.0.with_program(|p| {
            p.windows
                .lock()
                .unwrap()
                .iter()
                .for_each(|w| SpaceElement::output_leave(w, output))
        })
    }
    fn geometry(&self) -> Rectangle<i32, Logical> {
        self.0.with_program(|p| {
            let mut geo =
                SpaceElement::geometry(&p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)]);
            geo.size.h += TAB_HEIGHT;
            geo
        })
    }
    fn z_index(&self) -> u8 {
        self.0.with_program(|p| {
            SpaceElement::z_index(&p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)])
        })
    }
    fn refresh(&self) {
        SpaceElement::refresh(&self.0);
        self.0.with_program(|p| {
            let mut windows = p.windows.lock().unwrap();

            // don't let the stack become empty
            let active = windows[p.active.load(Ordering::SeqCst)].clone();
            windows.retain(IsAlive::alive);
            if windows.is_empty() {
                windows.push(active);
            }

            let len = windows.len();
            let _ = p
                .active
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |active| {
                    (active >= len).then_some(len - 1)
                });
            windows.iter().for_each(|w| SpaceElement::refresh(w));
        });
    }
}

impl KeyboardTarget<State> for CosmicStack {
    fn enter(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        self.0.with_program(|p| {
            let active = p.active.load(Ordering::SeqCst);
            p.previous_keyboard.store(active, Ordering::SeqCst);
            KeyboardTarget::enter(
                &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)],
                seat,
                data,
                keys,
                serial,
            )
        })
    }
    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial) {
        let active = self.keyboard_leave_if_previous(seat, data, serial);
        self.0.force_redraw();
        self.0.with_program(|p| {
            p.group_focused.store(false, Ordering::SeqCst);
            KeyboardTarget::leave(&p.windows.lock().unwrap()[active], seat, data, serial)
        })
    }
    fn key(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        let active = self.keyboard_leave_if_previous(seat, data, serial);
        self.0.with_program(|p| {
            if !p.group_focused.load(Ordering::SeqCst) {
                KeyboardTarget::key(
                    &p.windows.lock().unwrap()[active],
                    seat,
                    data,
                    key,
                    state,
                    serial,
                    time,
                )
            }
        })
    }
    fn modifiers(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        let active = self.keyboard_leave_if_previous(seat, data, serial);
        self.0.with_program(|p| {
            if !p.group_focused.load(Ordering::SeqCst) {
                KeyboardTarget::modifiers(
                    &p.windows.lock().unwrap()[active],
                    seat,
                    data,
                    modifiers,
                    serial,
                )
            }
        })
    }
}

impl PointerTarget<State> for CosmicStack {
    fn enter(&self, seat: &Seat<State>, data: &mut State, event: &MotionEvent) {
        let mut event = event.clone();
        event.location.y -= TAB_HEIGHT as f64;
        if self.0.with_program(|p| {
            let active_window = &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)];
            if let Some(sessions) = active_window.user_data().get::<ScreencopySessions>() {
                for session in &*sessions.0.borrow() {
                    session.cursor_enter(seat, InputType::Pointer)
                }
            }

            if (event.location.y - active_window.geometry().loc.y as f64) < 0. {
                let previous = p.swap_focus(Focus::Header);
                if previous == Focus::Window {
                    PointerTarget::leave(active_window, seat, data, event.serial, event.time);
                }
                true
            } else {
                p.swap_focus(Focus::Window);

                *p.last_location.lock().unwrap() = Some((event.location, event.serial, event.time));
                let active = p.active.load(Ordering::SeqCst);
                p.previous_pointer.store(active, Ordering::SeqCst);

                PointerTarget::enter(active_window, seat, data, &event);
                false
            }
        }) {
            event.location.y += TAB_HEIGHT as f64;
            event.location -= self.0.with_program(|p| {
                p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)]
                    .geometry()
                    .loc
                    .to_f64()
            });
            PointerTarget::enter(&self.0, seat, data, &event)
        }
    }

    fn motion(&self, seat: &Seat<State>, data: &mut State, event: &MotionEvent) {
        let mut event = event.clone();
        event.location.y -= TAB_HEIGHT as f64;
        let active =
            self.pointer_leave_if_previous(seat, data, event.serial, event.time, event.location);
        if let Some((previous, next)) = self.0.with_program(|p| {
            let active_window = &p.windows.lock().unwrap()[active];
            if let Some(sessions) = active_window.user_data().get::<ScreencopySessions>() {
                for session in &*sessions.0.borrow() {
                    let buffer_loc = (event.location.x, event.location.y); // we always screencast windows at 1x1 scale
                    if let Some((geo, hotspot)) =
                        seat.cursor_geometry(buffer_loc, data.common.clock.now())
                    {
                        session.cursor_info(seat, InputType::Pointer, geo, hotspot);
                    }
                }
            }

            if (event.location.y - active_window.geometry().loc.y as f64) < 0. {
                let previous = p.swap_focus(Focus::Header);
                if previous == Focus::Window {
                    PointerTarget::leave(active_window, seat, data, event.serial, event.time);
                }
                Some((previous, Focus::Header))
            } else {
                *p.last_location.lock().unwrap() = Some((event.location, event.serial, event.time));

                let previous = p.swap_focus(Focus::Window);
                if previous != Focus::Window {
                    PointerTarget::enter(active_window, seat, data, &event);
                } else {
                    PointerTarget::motion(active_window, seat, data, &event);
                }

                Some((previous, Focus::Window))
            }
        }) {
            event.location.y += TAB_HEIGHT as f64;
            event.location -= self
                .0
                .with_program(|p| p.windows.lock().unwrap()[active].geometry().loc.to_f64());
            match (previous, next) {
                (Focus::Header, Focus::Header) => {
                    PointerTarget::motion(&self.0, seat, data, &event)
                }
                (_, Focus::Header) => PointerTarget::enter(&self.0, seat, data, &event),
                (Focus::Header, _) => {
                    PointerTarget::leave(&self.0, seat, data, event.serial, event.time)
                }
                _ => {}
            }
        }
    }

    fn relative_motion(&self, seat: &Seat<State>, data: &mut State, event: &RelativeMotionEvent) {
        self.0.with_program(|p| {
            if p.current_focus() == Focus::Window {
                let window = &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)];
                window.relative_motion(seat, data, event)
            }
        })
    }

    fn button(&self, seat: &Seat<State>, data: &mut State, event: &ButtonEvent) {
        if let Some((location, _serial, _time)) = self
            .0
            .with_program(|p| p.last_location.lock().unwrap().clone())
        {
            self.pointer_leave_if_previous(seat, data, event.serial, event.time, location);
        }

        match self.0.with_program(|p| p.current_focus()) {
            Focus::Header => {
                self.0.with_program(|p| {
                    *p.last_seat.lock().unwrap() = Some((seat.clone(), event.serial));
                });
                PointerTarget::button(&self.0, seat, data, event)
            }
            Focus::Window => {
                if self.0.with_program(|p| {
                    PointerTarget::button(
                        &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)],
                        seat,
                        data,
                        event,
                    );
                    if p.group_focused.swap(false, Ordering::SeqCst) {
                        p.windows.lock().unwrap().iter().for_each(|w| {
                            SpaceElement::set_activate(w, true);
                            w.send_configure();
                        });
                        true
                    } else {
                        false
                    }
                }) {
                    self.0.force_redraw();
                }
            }
            _ => {}
        }
    }

    fn axis(&self, seat: &Seat<State>, data: &mut State, frame: AxisFrame) {
        if let Some((location, serial, time)) = self
            .0
            .with_program(|p| p.last_location.lock().unwrap().clone())
        {
            self.pointer_leave_if_previous(seat, data, serial, time, location);
        }

        match self.0.with_program(|p| p.current_focus()) {
            Focus::Header => PointerTarget::axis(&self.0, seat, data, frame),
            Focus::Window => self.0.with_program(|p| {
                PointerTarget::axis(
                    &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)],
                    seat,
                    data,
                    frame,
                )
            }),
            _ => {}
        }
    }

    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial, time: u32) {
        if let Some((location, serial, time)) = self
            .0
            .with_program(|p| p.last_location.lock().unwrap().clone())
        {
            self.pointer_leave_if_previous(seat, data, serial, time, location);
        }

        let previous = self.0.with_program(|p| {
            if let Some(sessions) = p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)]
                .user_data()
                .get::<ScreencopySessions>()
            {
                for session in &*sessions.0.borrow() {
                    session.cursor_leave(seat, InputType::Pointer)
                }
            }

            p.swap_focus(Focus::None)
        });

        match previous {
            Focus::Header => PointerTarget::leave(&self.0, seat, data, serial, time),
            Focus::Window => self.0.with_program(|p| {
                PointerTarget::leave(
                    &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)],
                    seat,
                    data,
                    serial,
                    time,
                )
            }),
            _ => {}
        }
    }
}

render_elements! {
    pub CosmicStackRenderElement<R> where R: ImportAll + ImportMem;
    Header = MemoryRenderBufferRenderElement<R>,
    Window = WaylandSurfaceRenderElement<R>,
}

impl<R> AsRenderElements<R> for CosmicStack
where
    R: Renderer + ImportAll + ImportMem,
    <R as Renderer>::TextureId: 'static,
{
    type RenderElement = CosmicStackRenderElement<R>;
    fn render_elements<C: From<Self::RenderElement>>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C> {
        let stack_loc = location
            + self
                .0
                .with_program(|p| {
                    p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)]
                        .geometry()
                        .loc
                })
                .to_physical_precise_round(scale);
        let window_loc = location + Point::from((0, (TAB_HEIGHT as f64 * scale.y) as i32));

        let mut elements = AsRenderElements::<R>::render_elements::<CosmicStackRenderElement<R>>(
            &self.0, renderer, stack_loc, scale, alpha,
        );

        elements.extend(self.0.with_program(|p| {
            AsRenderElements::<R>::render_elements::<CosmicStackRenderElement<R>>(
                &p.windows.lock().unwrap()[p.active.load(Ordering::SeqCst)],
                renderer,
                window_loc,
                scale,
                alpha,
            )
        }));

        elements.into_iter().map(C::from).collect()
    }
}
