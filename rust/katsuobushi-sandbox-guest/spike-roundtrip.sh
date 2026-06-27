#!/usr/bin/env bash
# Spike harness for the Katsuobushi sandbox controller server (design §9.6, minus live Claude).
#
# Plays all three peers around the server: Claude (stdio MCP client), the host
# operator (control unix socket, standing in for vsock), and the agent (`report`
# command). Proves the part of the channel contract we own:
#   1. the server declares `claude/channel` in its real `initialize` result;
#   2. a host-pushed Prompt becomes the exact `notifications/claude/channel`
#      JSON-RPC the docs specify, with our content + meta.turn_id;
#   3. a `report` line round-trips back to the host over the control connection.
set -u

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# Defaults to the cargo debug build; override with KATSU_SERVER_BIN to run
# against the reproducible Nix-built binary. The `report` command is now a thin
# shell app (jq+socat), so this harness writes the report line straight to the
# socket rather than depending on a separate binary.
BIN="${KATSU_SERVER_BIN:-$ROOT/target/debug/katsuobushi-sandbox-control}"
TMP="$(mktemp -d)"
trap 'kill ${SRV:-} ${SOCAT:-} 2>/dev/null; rm -rf "$TMP"' EXIT

export KATSU_CONTROL_UNIX="$TMP/control.sock"
export KATSU_REPORT_SOCK="$TMP/report.sock"

mkfifo "$TMP/server_in"
"$BIN" < "$TMP/server_in" > "$TMP/server_out" 2> "$TMP/server_err" &
SRV=$!
exec 3> "$TMP/server_in"   # hold the server's stdin open

# --- 1. MCP handshake (we are Claude) ---------------------------------------
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"spike","version":"0"}}}' >&3
printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}' >&3
sleep 1

# --- 2. Connect as the host over the control socket -------------------------
mkfifo "$TMP/host_in"
socat "UNIX-CONNECT:$KATSU_CONTROL_UNIX" - < "$TMP/host_in" > "$TMP/ctrl_out" 2>/dev/null &
SOCAT=$!
exec 4> "$TMP/host_in"
sleep 0.5
printf '%s\n' '{"type":"prompt","turn_id":7,"text":"hello from host"}' >&4
sleep 1

# --- 3. Agent reports (the `report` shell app writes exactly this line) ------
printf '%s\n' '{"status":"done","text":"spike works","turn_id":7}' \
  | socat - "UNIX-CONNECT:$KATSU_REPORT_SOCK"
sleep 1

exec 3>&- 4>&-
sleep 0.3

echo "===== server stdout (MCP JSON-RPC → Claude) ====="; cat "$TMP/server_out"
echo; echo "===== control socket (server → host) ====="; cat "$TMP/ctrl_out"
echo; echo "===== server stderr ====="; cat "$TMP/server_err"
echo; echo "===== ASSERTIONS ====="
chk(){ if grep -q "$2" "$1"; then echo "PASS: $3"; else echo "FAIL: $3"; fi; }
chk "$TMP/server_out" '"claude/channel"'              'initialize result declares claude/channel capability'
chk "$TMP/server_out" 'notifications/claude/channel'  'server emits notifications/claude/channel to Claude'
chk "$TMP/server_out" 'hello from host'               'pushed prompt content reaches the channel notification'
chk "$TMP/server_out" '"turn_id":"7"'                 'meta.turn_id carried as a string attribute'
chk "$TMP/ctrl_out"   '"type":"ready"'                'host receives the one-shot ready'
chk "$TMP/ctrl_out"   'spike works'                   'report relayed back to host over control connection'
