//! Keyboard input through the virtual-keyboard protocol.
//!
//! We upload our own keymap that maps each needed keysym to some keycode. Apps
//! like Chrome also interpret the keycode as a physical key, so assigning codes
//! naively (as wtype does) types "ö" on the Backspace key, deleting text, or "\""
//! on Left Ctrl, typing nothing. Text characters therefore only use codes of
//! ordinary character keys, and named keys (Return, F5, ...) use their real codes.

use std::collections::HashMap;
use std::io::Write as _;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rustix::fs::{MemfdFlags, memfd_create};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1 as Manager;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1 as VirtualKeyboard;

use crate::sway::wayland_socket;

pub const SHIFT: u32 = 1;
pub const CTRL: u32 = 4;
pub const ALT: u32 = 8;

const KEYMAP_FORMAT_XKB_V1: u32 = 1;
/// Time for the focused client to apply a new keymap before keys arrive.
const SETTLE: Duration = Duration::from_millis(40);
const KEY_GAP: Duration = Duration::from_millis(6);

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
        if let wl_registry::Event::Global { name, interface, .. } = event {
            match interface.as_str() {
                "zwp_virtual_keyboard_manager_v1" => {
                    state.manager = Some(registry.bind::<Manager, _, _>(name, 1, qh, ()));
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
delegate_noop!(Globals: VirtualKeyboard);

/// evdev codes of plain character keys (1-0 -= / q-] / a-` / \\-/), safe to reassign to any symbol.
const POOL: &[u32] = &[
    2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 30, 31, 32, 33, 34,
    35, 36, 37, 38, 39, 40, 41, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53,
];

/// Real evdev code for keys whose physical identity matters to apps.
fn natural_code(keysym: &str) -> Option<u32> {
    Some(match keysym {
        "Escape" => 1,
        "BackSpace" => 14,
        "Tab" => 15,
        "Return" => 28,
        "space" => 57,
        "Home" => 102,
        "Up" => 103,
        "Prior" => 104,
        "Left" => 105,
        "Right" => 106,
        "End" => 107,
        "Down" => 108,
        "Next" => 109,
        "Insert" => 110,
        "Delete" => 111,
        f if f.starts_with('F') => match f[1..].parse::<u32>().ok()? {
            n @ 1..=10 => 58 + n,
            11 => 87,
            12 => 88,
            _ => return None,
        },
        _ => return None,
    })
}

/// One key to send: an xkb keysym name plus modifier mask.
#[derive(Clone)]
pub struct Stroke {
    pub keysym: String,
    pub mods: u32,
}

/// Keysym name for a character typed as text.
fn char_keysym(c: char) -> String {
    match c {
        '\n' => "Return".to_owned(),
        '\t' => "Tab".to_owned(),
        ' ' => "space".to_owned(),
        c => format!("U{:04X}", c as u32),
    }
}

pub fn text_strokes(text: &str) -> Vec<Stroke> {
    text.chars().map(|c| Stroke { keysym: char_keysym(c), mods: 0 }).collect()
}

/// Keysym for a key name in a chord: single characters by code point, otherwise an xkb name like "Return".
pub fn key_keysym(name: &str) -> Result<String> {
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Ok(char_keysym(c.to_ascii_lowercase())),
        _ if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => Ok(name.to_owned()),
        _ => bail!("invalid key name {name:?}"),
    }
}

pub struct Keyboard {
    conn: Connection,
    queue: EventQueue<Globals>,
    globals: Globals,
    kbd: VirtualKeyboard,
    epoch: Instant,
}

impl Keyboard {
    pub fn connect() -> Result<Keyboard> {
        let stream = UnixStream::connect(wayland_socket()?).context("connecting to the Wayland display")?;
        let conn = Connection::from_socket(stream)?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut globals = Globals::default();
        queue.roundtrip(&mut globals)?;
        let (Some(manager), Some(seat)) = (&globals.manager, &globals.seat) else {
            return Err(anyhow!("compositor lacks zwp_virtual_keyboard_manager_v1 or a seat"));
        };
        let kbd = manager.create_virtual_keyboard(seat, &qh, ());
        queue.roundtrip(&mut globals)?;
        Ok(Keyboard { conn, queue, globals, kbd, epoch: Instant::now() })
    }

    fn now(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    /// Upload a keymap for `keysyms` (at most POOL.len() without a natural code); returns evdev codes.
    fn upload_keymap(&mut self, keysyms: &[&str]) -> Result<HashMap<String, u32>> {
        let mut codes: HashMap<String, u32> = HashMap::new();
        let mut pool = POOL.iter();
        for sym in keysyms {
            if !codes.contains_key(*sym) {
                let code = match natural_code(sym) {
                    Some(c) => c,
                    None => *pool.next().ok_or_else(|| anyhow!("too many distinct symbols for one keymap"))?,
                };
                codes.insert(sym.to_string(), code);
            }
        }
        let mut keycodes = String::new();
        let mut symbols = String::new();
        for (sym, code) in &codes {
            keycodes += &format!("<K{code}> = {};\n", code + 8);
            symbols += &format!("key <K{code}> {{[ {sym} ]}};\n");
        }
        let keymap = format!(
            "xkb_keymap {{\nxkb_keycodes \"cu\" {{\nminimum = 8;\nmaximum = 255;\n{keycodes}}};\n\
             xkb_types \"cu\" {{ include \"complete\" }};\nxkb_compatibility \"cu\" {{ include \"complete\" }};\n\
             xkb_symbols \"cu\" {{\n{symbols}}};\n}};\n"
        );
        let fd = memfd_create("cu-keymap", MemfdFlags::CLOEXEC)?;
        let mut file = std::fs::File::from(fd);
        file.write_all(keymap.as_bytes())?;
        file.write_all(&[0])?;
        self.kbd.keymap(KEYMAP_FORMAT_XKB_V1, file.as_fd(), keymap.len() as u32 + 1);
        // Clear modifiers too: this makes the virtual keyboard the seat's active one, which
        // is when sway forwards our keymap to the focused client.
        self.kbd.modifiers(0, 0, 0, 0);
        self.queue.roundtrip(&mut self.globals).context("Wayland connection lost")?;
        Ok(codes)
    }

    /// Send strokes, one keymap per batch of at most POOL.len() distinct symbols. Blocks for the duration.
    pub fn send(&mut self, strokes: &[Stroke]) -> Result<()> {
        let mut rest = strokes;
        while !rest.is_empty() {
            let mut seen = std::collections::HashSet::new();
            let n = rest
                .iter()
                .position(|s| {
                    natural_code(&s.keysym).is_none() && seen.insert(s.keysym.as_str()) && seen.len() > POOL.len()
                })
                .unwrap_or(rest.len());
            let (batch, tail) = rest.split_at(n);
            self.send_batch(batch)?;
            rest = tail;
        }
        Ok(())
    }

    fn send_batch(&mut self, strokes: &[Stroke]) -> Result<()> {
        let syms: Vec<&str> = strokes.iter().map(|s| s.keysym.as_str()).collect();
        let codes = self.upload_keymap(&syms)?;
        std::thread::sleep(SETTLE);
        let mut mods = 0;
        for s in strokes {
            if s.mods != mods {
                self.kbd.modifiers(s.mods, 0, 0, 0);
                mods = s.mods;
            }
            let code = codes[&s.keysym];
            self.kbd.key(self.now(), code, 1);
            self.conn.flush()?;
            std::thread::sleep(KEY_GAP);
            self.kbd.key(self.now(), code, 0);
            self.conn.flush()?;
            std::thread::sleep(KEY_GAP);
        }
        if mods != 0 {
            self.kbd.modifiers(0, 0, 0, 0);
        }
        self.queue.roundtrip(&mut self.globals).context("Wayland connection lost")?;
        Ok(())
    }
}
