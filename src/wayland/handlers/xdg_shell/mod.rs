// SPDX-License-Identifier: GPL-3.0-only

use crate::{shell::CosmicSurface, utils::prelude::*, wayland::protocols::screencopy::SessionType};
use smithay::{
    delegate_xdg_shell,
    desktop::{
        find_popup_root_surface, PopupGrab, PopupKeyboardGrab, PopupKind, PopupPointerGrab,
        PopupUngrabStrategy, Window,
    },
    input::{pointer::Focus, Seat},
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::{wl_output::WlOutput, wl_seat::WlSeat},
    },
    utils::Serial,
    wayland::{
        seat::WaylandFocus,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
    },
};
use std::cell::Cell;
use tracing::warn;

use super::screencopy::PendingScreencopyBuffers;

pub mod popup;

pub type PopupGrabData = Cell<Option<PopupGrab<State>>>;

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.common.shell.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let seat = self.common.last_active_seat().clone();
        let window = CosmicSurface::Wayland(Window::new(surface));
        self.common.shell.pending_windows.push((window, seat));
        // We will position the window after the first commit, when we know its size hints
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });

        if surface.get_parent_surface().is_some() {
            // let other shells deal with their popups
            self.common.shell.unconstrain_popup(&surface, &positioner);

            if surface.send_configure().is_ok() {
                self.common
                    .shell
                    .popups
                    .track_popup(PopupKind::from(surface))
                    .unwrap();
            }
        }
    }

    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        let seat = Seat::from_resource(&seat).unwrap();
        let kind = PopupKind::Xdg(surface);
        if let Some(root) = find_popup_root_surface(&kind)
            .ok()
            .and_then(|root| self.common.shell.element_for_wl_surface(&root))
        {
            let target = root.clone().into();
            let ret = self
                .common
                .shell
                .popups
                .grab_popup(target, kind, &seat, serial);

            if let Ok(mut grab) = ret {
                if let Some(keyboard) = seat.get_keyboard() {
                    if keyboard.is_grabbed()
                        && !(keyboard.has_grab(serial)
                            || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
                    {
                        grab.ungrab(PopupUngrabStrategy::All);
                        return;
                    }
                    Common::set_focus(self, grab.current_grab().as_ref(), &seat, Some(serial));
                    keyboard.set_grab(PopupKeyboardGrab::new(&grab), serial);
                }

                if let Some(pointer) = seat.get_pointer() {
                    if pointer.is_grabbed()
                        && !(pointer.has_grab(serial)
                            || pointer
                                .has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
                    {
                        grab.ungrab(PopupUngrabStrategy::All);
                        return;
                    }
                    pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
                }

                seat.user_data()
                    .insert_if_missing(|| PopupGrabData::new(None));
                seat.user_data()
                    .get::<PopupGrabData>()
                    .unwrap()
                    .set(Some(grab));
            }
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });

        self.common.shell.unconstrain_popup(&surface, &positioner);
        surface.send_repositioned(token);
        if let Err(err) = surface.send_configure() {
            warn!(
                ?err,
                "Client bug: Unable to re-configure repositioned popup.",
            );
        }
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: WlSeat, serial: Serial) {
        let seat = Seat::from_resource(&seat).unwrap();
        Shell::move_request(self, surface.wl_surface(), &seat, serial)
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let seat = Seat::from_resource(&seat).unwrap();
        Shell::resize_request(self, surface.wl_surface(), &seat, serial, edges.into())
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        let seat = self.common.last_active_seat();
        let output = seat.active_output();

        if let Some(mapped) = self
            .common
            .shell
            .element_for_wl_surface(surface.wl_surface())
            .cloned()
        {
            if let Some(workspace) = self.common.shell.space_for_mut(&mapped) {
                let (window, _) = mapped
                    .windows()
                    .find(|(w, _)| w.wl_surface().as_ref() == Some(surface.wl_surface()))
                    .unwrap();
                workspace.maximize_request(&window, &output)
            }
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(mapped) = self
            .common
            .shell
            .element_for_wl_surface(surface.wl_surface())
            .cloned()
        {
            if let Some(workspace) = self.common.shell.space_for_mut(&mapped) {
                let (window, _) = mapped
                    .windows()
                    .find(|(w, _)| w.wl_surface().as_ref() == Some(surface.wl_surface()))
                    .unwrap();
                workspace.unmaximize_request(&window);
            }
        }
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, output: Option<WlOutput>) {
        let output = output
            .as_ref()
            .and_then(Output::from_resource)
            .unwrap_or_else(|| {
                let seat = self.common.last_active_seat();
                seat.active_output()
            });

        if let Some(mapped) = self
            .common
            .shell
            .element_for_wl_surface(surface.wl_surface())
            .cloned()
        {
            if let Some(workspace) = self.common.shell.space_for_mut(&mapped) {
                let (window, _) = mapped
                    .windows()
                    .find(|(w, _)| w.wl_surface().as_ref() == Some(surface.wl_surface()))
                    .unwrap();
                workspace.fullscreen_request(&window, &output)
            }
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(mapped) = self
            .common
            .shell
            .element_for_wl_surface(surface.wl_surface())
            .cloned()
        {
            if let Some(workspace) = self.common.shell.space_for_mut(&mapped) {
                let (window, _) = mapped
                    .windows()
                    .find(|(w, _)| w.wl_surface().as_ref() == Some(surface.wl_surface()))
                    .unwrap();
                workspace.unfullscreen_request(&window)
            }
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let outputs = self
            .common
            .shell
            .visible_outputs_for_surface(surface.wl_surface())
            .collect::<Vec<_>>();
        for output in outputs.iter() {
            self.common.shell.active_space_mut(output).refresh();
        }

        // screencopy
        let mut scheduled_sessions = self.schedule_workspace_sessions(surface.wl_surface());
        for output in outputs.into_iter() {
            if let Some(sessions) = output.user_data().get::<PendingScreencopyBuffers>() {
                scheduled_sessions
                    .get_or_insert_with(Vec::new)
                    .extend(sessions.borrow_mut().drain(..));
            }
            self.backend.schedule_render(
                &self.common.event_loop_handle,
                &output,
                scheduled_sessions.as_ref().map(|sessions| {
                    sessions
                        .iter()
                        .filter(|(s, _)| match s.session_type() {
                            SessionType::Output(o) | SessionType::Workspace(o, _)
                                if o == output =>
                            {
                                true
                            }
                            _ => false,
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                }),
            );
        }
    }
}

delegate_xdg_shell!(State);
