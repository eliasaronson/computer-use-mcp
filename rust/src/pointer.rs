//! Pointer input through the wlr virtual-pointer protocol.
//!
//! sway's `seat cursor` IPC commands can't drag (no motion is delivered while a
//! button is held) and scroll by ~2px per notch, so we act as a real pointer device.

use std::os::unix::net::UnixStream;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 as Manager;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1 as VirtualPointer;

use crate::sway::{Rect, wayland_socket};

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

/// Scroll distance per wheel notch, matching libinput's default for a physical wheel.
const NOTCH: f64 = 15.0;

#[derive(Default)]
struct Globals {
    manager: Option<Manager>,
    seat: Option<WlSeat>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "zwlr_virtual_pointer_manager_v1" => {
                    state.manager = Some(registry.bind::<Manager, _, _>(name, version.min(2), qh, ()));
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind::<WlSeat, _, _>(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(Globals: ignore WlSeat);
delegate_noop!(Globals: Manager);
delegate_noop!(Globals: VirtualPointer);

pub enum Scroll {
    Up,
    Down,
    Left,
    Right,
}

pub struct Pointer {
    conn: Connection,
    _queue: EventQueue<Globals>,
    ptr: VirtualPointer,
    epoch: Instant,
}

impl Pointer {
    pub fn connect() -> Result<Pointer> {
        let stream = UnixStream::connect(wayland_socket()?).context("connecting to the Wayland display")?;
        let conn = Connection::from_socket(stream)?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut globals = Globals::default();
        queue.roundtrip(&mut globals)?;
        let manager = globals
            .manager
            .as_ref()
            .ok_or_else(|| anyhow!("compositor lacks zwlr_virtual_pointer_manager_v1"))?;
        let ptr = manager.create_virtual_pointer(globals.seat.as_ref(), &qh, ());
        queue.roundtrip(&mut globals)?;
        Ok(Pointer { conn, _queue: queue, ptr, epoch: Instant::now() })
    }

    fn now(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    fn flush(&self) -> Result<()> {
        self.conn.flush().context("Wayland connection lost")?;
        Ok(())
    }

    /// Warp to a point in sway layout coordinates.
    pub fn move_to(&self, x: i32, y: i32, layout: Rect) -> Result<()> {
        let t = self.now();
        self.ptr.motion_absolute(
            t,
            (x - layout.x).clamp(0, layout.w - 1) as u32,
            (y - layout.y).clamp(0, layout.h - 1) as u32,
            layout.w as u32,
            layout.h as u32,
        );
        self.ptr.frame();
        self.flush()
    }

    pub fn button(&self, button: u32, pressed: bool) -> Result<()> {
        let state = if pressed { ButtonState::Pressed } else { ButtonState::Released };
        self.ptr.button(self.now(), button, state);
        self.ptr.frame();
        self.flush()
    }

    pub fn scroll(&self, dir: &Scroll, notches: u32) -> Result<()> {
        let (axis, sign) = match dir {
            Scroll::Up => (Axis::VerticalScroll, -1),
            Scroll::Down => (Axis::VerticalScroll, 1),
            Scroll::Left => (Axis::HorizontalScroll, -1),
            Scroll::Right => (Axis::HorizontalScroll, 1),
        };
        for _ in 0..notches {
            let t = self.now();
            self.ptr.axis_source(AxisSource::Wheel);
            self.ptr.axis_discrete(t, axis, NOTCH * sign as f64, sign);
            self.ptr.frame();
        }
        self.flush()
    }
}
