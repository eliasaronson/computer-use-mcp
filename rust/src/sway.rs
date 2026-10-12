//! Minimal sway IPC client plus the window model the safety checks run on.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::config::Policy;

const MAGIC: &[u8] = b"i3-ipc";
const RUN_COMMAND: u32 = 0;
const GET_WORKSPACES: u32 = 1;
const GET_OUTPUTS: u32 = 3;
const GET_TREE: u32 = 4;

/// Runtime directory of the current user (`$XDG_RUNTIME_DIR`).
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw())))
}

/// Newest file in the runtime dir matching `prefix*suffix` (survives sway restarts / re-logins).
fn newest(prefix: &str, suffix: &str) -> Option<PathBuf> {
    std::fs::read_dir(runtime_dir())
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(prefix) && n.ends_with(suffix)
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// The sway IPC socket of the running session.
pub fn sway_socket() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("SWAYSOCK").map(PathBuf::from)
        && p.exists()
    {
        return Ok(p);
    }
    newest("sway-ipc.", ".sock").ok_or_else(|| anyhow!("no running sway session found"))
}

/// The Wayland display socket of the running session.
pub fn wayland_socket() -> Result<PathBuf> {
    if let Some(name) = std::env::var_os("WAYLAND_DISPLAY") {
        let p = runtime_dir().join(name);
        if p.exists() {
            return Ok(p);
        }
    }
    newest("wayland-", "")
        .filter(|p| !p.to_string_lossy().ends_with(".lock"))
        .ok_or_else(|| anyhow!("no wayland display found"))
}

async fn ipc(kind: u32, payload: &str) -> Result<Value> {
    let mut s = UnixStream::connect(sway_socket()?).await.context("connecting to sway IPC")?;
    let mut msg = Vec::with_capacity(14 + payload.len());
    msg.extend_from_slice(MAGIC);
    msg.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    msg.extend_from_slice(&kind.to_ne_bytes());
    msg.extend_from_slice(payload.as_bytes());
    s.write_all(&msg).await?;
    let mut header = [0u8; 14];
    s.read_exact(&mut header).await?;
    if &header[..6] != MAGIC {
        bail!("bad sway IPC reply");
    }
    let len = u32::from_ne_bytes(header[6..10].try_into()?) as usize;
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

pub async fn command(cmd: &str) -> Result<()> {
    let reply = ipc(RUN_COMMAND, cmd).await?;
    for r in reply.as_array().into_iter().flatten() {
        if r["success"] != Value::Bool(true) {
            bail!("sway refused {cmd:?}: {}", r["error"].as_str().unwrap_or("unknown error"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    fn of(v: &Value) -> Rect {
        let n = |k: &str| v[k].as_i64().unwrap_or(0) as i32;
        Rect { x: n("x"), y: n("y"), w: n("width"), h: n("height") }
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.x <= x && x < self.x + self.w && self.y <= y && y < self.y + self.h
    }
}

#[derive(Clone, Debug)]
pub struct Output {
    pub name: String,
    pub rect: Rect,
    pub focused: bool,
}

pub async fn outputs() -> Result<Vec<Output>> {
    let v = ipc(GET_OUTPUTS, "").await?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["active"] == Value::Bool(true))
        .map(|o| Output {
            name: o["name"].as_str().unwrap_or_default().to_owned(),
            rect: Rect::of(&o["rect"]),
            focused: o["focused"] == Value::Bool(true),
        })
        .collect())
}

/// Bounding box of all outputs, which is the space virtual-pointer absolute motion maps onto.
pub fn layout_box(outs: &[Output]) -> Rect {
    let x0 = outs.iter().map(|o| o.rect.x).min().unwrap_or(0);
    let y0 = outs.iter().map(|o| o.rect.y).min().unwrap_or(0);
    let x1 = outs.iter().map(|o| o.rect.x + o.rect.w).max().unwrap_or(1);
    let y1 = outs.iter().map(|o| o.rect.y + o.rect.h).max().unwrap_or(1);
    Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}

#[derive(Clone, Debug)]
pub struct Window {
    /// sway con id; negative for tab/stack headers of plain containers (pseudo-windows).
    pub id: i64,
    pub app_id: String,
    pub title: String,
    pub rect: Rect,
    pub output: String,
    pub workspace: String,
    pub visible: bool,
    pub focused: bool,
    pub floating: bool,
    /// Title bar / tab header in layout coordinates, only while its workspace is shown.
    pub deco: Option<Rect>,
    pub blocked: bool,
    pub private: bool,
}

pub struct Desktop {
    pub windows: Vec<Window>,
    /// Usable (non-bar) area of the shown workspace, per output.
    pub usable: HashMap<String, Rect>,
}

fn app_id(node: &Value) -> String {
    let props = &node["window_properties"];
    [&node["app_id"], &props["class"], &props["instance"]]
        .into_iter()
        .find_map(|v| v.as_str().filter(|s| !s.is_empty()))
        .unwrap_or_default()
        .to_owned()
}

fn children(node: &Value) -> impl Iterator<Item = (&Value, bool)> {
    let tiled = node["nodes"].as_array().into_iter().flatten().map(|c| (c, false));
    let floating = node["floating_nodes"].as_array().into_iter().flatten().map(|c| (c, true));
    tiled.chain(floating)
}

fn leaves(node: &Value) -> Vec<&Value> {
    if !node["pid"].is_null() {
        return vec![node];
    }
    children(node).flat_map(|(c, _)| leaves(c)).collect()
}

impl Desktop {
    pub async fn query(policy: &Policy) -> Result<Desktop> {
        let mut shown = HashMap::new();
        for ws in ipc(GET_WORKSPACES, "").await?.as_array().into_iter().flatten() {
            if ws["visible"] == Value::Bool(true) {
                let out = ws["output"].as_str().unwrap_or_default().to_owned();
                let name = ws["name"].as_str().unwrap_or_default().to_owned();
                shown.insert((out, name), Rect::of(&ws["rect"]));
            }
        }
        let usable = shown.iter().map(|((out, _), r)| (out.clone(), *r)).collect();
        let tree = ipc(GET_TREE, "").await?;
        let mut windows = Vec::new();
        walk(&tree, None, "", "", false, &shown, policy, &mut windows);
        Ok(Desktop { windows, usable })
    }

    pub fn focused(&self) -> Option<&Window> {
        self.windows.iter().find(|w| w.focused)
    }

    /// Topmost window (content or title bar / tab header) at a layout point.
    pub fn window_at(&self, x: i32, y: i32) -> Option<&Window> {
        let hit = |w: &&Window| (w.visible && w.rect.contains(x, y)) || w.deco.is_some_and(|d| d.contains(x, y));
        // floating windows are stacked above tiled ones
        self.windows
            .iter()
            .filter(|w| w.floating)
            .find(hit)
            .or_else(|| self.windows.iter().filter(|w| !w.floating).find(hit))
    }

    /// What's on an output: visible windows and their geometry, in tree order.
    pub fn layout_sig(&self, output: &str) -> Vec<(i64, bool, Rect)> {
        self.windows
            .iter()
            .filter(|w| w.visible && w.output == output)
            .map(|w| (w.id, w.floating, w.rect))
            .collect()
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    node: &Value,
    parent: Option<Rect>,
    output: &str,
    workspace: &str,
    floating: bool,
    shown: &HashMap<(String, String), Rect>,
    policy: &Policy,
    out: &mut Vec<Window>,
) {
    let kind = node["type"].as_str().unwrap_or_default();
    let name = node["name"].as_str().unwrap_or_default();
    let (output, workspace) = match kind {
        "output" => (name, workspace),
        "workspace" => (output, name),
        _ => (output, workspace),
    };
    let deco = parent.and_then(|p| {
        let d = Rect::of(&node["deco_rect"]);
        (d.w > 0 && d.h > 0 && shown.contains_key(&(output.to_owned(), workspace.to_owned())))
            .then(|| Rect { x: p.x + d.x, y: p.y + d.y, w: d.w, h: d.h })
    });
    if kind == "con" || kind == "floating_con" {
        let id = node["id"].as_i64().unwrap_or(0);
        if !node["pid"].is_null() {
            let app = app_id(node);
            out.push(Window {
                id,
                blocked: policy.blocked(&app, name),
                private: policy.private(&app, name),
                app_id: app,
                title: name.to_owned(),
                rect: Rect::of(&node["rect"]),
                output: output.to_owned(),
                workspace: workspace.to_owned(),
                visible: node["visible"] == Value::Bool(true),
                focused: node["focused"] == Value::Bool(true),
                floating,
                deco,
            });
        } else if let Some(deco) = deco {
            // Header of a tabbed/stacked container: clicking it can bring any child forward.
            let inside = leaves(node);
            out.push(Window {
                id: -id,
                app_id: format!("<container: {}>", inside.iter().map(|n| app_id(n)).collect::<Vec<_>>().join(", ")),
                title: String::new(),
                rect: Rect { x: 0, y: 0, w: 0, h: 0 },
                output: output.to_owned(),
                workspace: workspace.to_owned(),
                visible: false,
                focused: false,
                floating,
                deco: Some(deco),
                blocked: inside.iter().any(|n| policy.blocked(&app_id(n), n["name"].as_str().unwrap_or_default())),
                private: false,
            });
        }
    }
    let rect = Rect::of(&node["rect"]);
    for (child, is_floating) in children(node) {
        walk(child, Some(rect), output, workspace, floating || is_floating, shown, policy, out);
    }
}
