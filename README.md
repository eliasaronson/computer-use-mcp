# computer-use-mcp

A shared MCP server that lets Claude Code sessions see and control this sway desktop.
One process (~14 MB) serves every session over HTTP on `127.0.0.1:47321`.

- Screenshots: `grim`, one monitor at a time, scaled to fit 1366x816
- Pointer: wlr virtual-pointer protocol (real drag and wheel scrolling)
- Keyboard: virtual-keyboard protocol with a per-call keymap
- Window model and checks: sway IPC

Tools: `screenshot`, `list_windows`, `focus_window`, `click`, `drag`, `scroll`,
`type_text`, `key`, `release_control`. Input tools take `screenshot: true` to also return
a screenshot taken 0.5 s after the action, which saves a round trip per step.

## Safety rails

- A bearer token is required on every request. Requests with a foreign `Host` or any
  browser `Origin` are rejected.
- Input to apps matching `[blocked]` in `config.toml` (terminals, password managers,
  Signal) is refused. This includes clicks on their title bars and tab headers.
- Apps matching `[private]` are blacked out of screenshots, and their titles are
  hidden in `list_windows`.
- Actions are refused if the screen changed since the caller's last screenshot.
- Clicks and drags are also refused if the pixels around the target changed, for example a
  banner pushed the button down or a dialog opened. The comparison uses block-averaged
  brightness of a fresh capture, so a blinking caret doesn't trigger it.
- Typing only goes to a window that was focused in the screenshot or clicked since.
- `super+…` and `ctrl+alt+F*/Backspace/Delete` are refused.
- One session controls the desktop at a time. The lease lasts 90 s after its last action.
- Every action is logged to `~/.local/state/computer-use-mcp/actions.jsonl` (mode 600).

Not covered: notifications (they appear in screenshots), and a window that pops up
between the check and the action.

## Session identity

The 2026-07-28 MCP protocol has no sessions. Instead, `headers-helper.sh` (Claude
Code's `headersHelper`, run once per connection) sends an `X-Claude-Session`
header made of `$CLAUDE_CODE_SESSION_ID` plus the PID of the calling Claude process.
It also reads the token from `~/.config/computer-use-mcp/token`, so the token never
sits in `~/.claude.json`.

## Install

```bash
cd ~/Documents/computer_use/rust && cargo build --release

# service
mkdir -p ~/.local/state/computer-use-mcp
ln -sf ~/Documents/computer_use/computer-use-mcp.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now computer-use-mcp

# kill switch: $mod+Escape stops the server
echo 'bindsym $mod+Escape exec systemctl --user stop computer-use-mcp' >> ~/.config/sway/config
swaymsg reload

# register for all Claude Code sessions
claude mcp add-json --scope user computer \
  '{"type":"http","url":"http://127.0.0.1:47321/mcp","headersHelper":"'"$HOME"'/Documents/computer_use/headers-helper.sh"}'
```

To be asked before each input action, add this to `permissions.ask` in
`~/.claude/settings.json`:
`mcp__computer__click`, `mcp__computer__drag`, `mcp__computer__scroll`,
`mcp__computer__type_text`, `mcp__computer__key`, `mcp__computer__focus_window`.

After changing `config.toml`, run `systemctl --user restart computer-use-mcp`.

### NixOS

On NixOS you can instead build the package with `buildRustPackage` and declare the
user service and policy in your system config. Then the binary and `config.toml` sit in
the read-only Nix store, so a Claude session can't loosen its own rules. With that
setup, the repo's `config.toml` and `computer-use-mcp.service` are only used for
development.
