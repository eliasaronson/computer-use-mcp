#!/bin/sh
# Claude Code `headersHelper` for the computer-use MCP server. Runs once per connection.
#  - Authorization: read from a chmod-600 file so the token never sits in ~/.claude.json
#  - X-Claude-Session: identifies the calling Claude Code process, because the
#    sessionless MCP protocol (2026-07-28) gives the server no other way to tell
#    concurrent sessions apart. CLAUDE_CODE_SESSION_ID alone is inherited by nested
#    sessions, so the PID of the Claude process that spawned us is added.
set -eu
token=$(cat "${CU_TOKEN_FILE:-$HOME/.config/computer-use-mcp/token}")
pid=$PPID
case "$(ps -o comm= -p "$pid")" in
  sh|bash|zsh|dash) pid=$(ps -o ppid= -p "$pid" | tr -d ' ') ;;
esac
printf '{"Authorization":"Bearer %s","X-Claude-Session":"%s/%s"}\n' \
  "$token" "${CLAUDE_CODE_SESSION_ID:-unknown}" "$pid"
