#!/bin/sh
# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=10

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/herdr-claude-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session) ;;
  *) exit 0 ;;
esac

if [ "${CLAUDE_CODE_SESSION_KIND:-}" != "bg" ]; then
  [ "${HERDR_ENV:-}" = "1" ] || exit 0
  [ -n "${HERDR_SOCKET_PATH:-}" ] || exit 0
  [ -n "${HERDR_PANE_ID:-}" ] || exit 0
fi
command -v python3 >/dev/null 2>&1 || exit 0

HERDR_ACTION="$action" HERDR_HOOK_INPUT_FILE="$hook_input_file" python3 - <<'PY'
import json
import os
import random
import socket
import subprocess
import time

source = "herdr:claude"
action = os.environ.get("HERDR_ACTION", "")
pane_id = os.environ.get("HERDR_PANE_ID")
socket_path = os.environ.get("HERDR_SOCKET_PATH")
hook_input_file = os.environ.get("HERDR_HOOK_INPUT_FILE")

hook_input = {}
if hook_input_file:
    try:
        with open(hook_input_file, encoding="utf-8") as handle:
            content = handle.read()
        if content.strip():
            hook_input = json.loads(content)
    except Exception:
        hook_input = {}

if "CURSOR_VERSION" in os.environ or "cursor_version" in hook_input:
    raise SystemExit(0)
hook_event_name = str(hook_input.get("hook_event_name") or "")
if hook_event_name != "SessionStart":
    raise SystemExit(0)
is_subagent = bool(hook_input.get("agent_id"))
if is_subagent:
    raise SystemExit(0)


def herdr_env(pid):
    try:
        with open(f"/proc/{pid}/environ", "rb") as handle:
            entries = handle.read().decode(errors="replace").split("\0")
    except OSError:
        try:
            entries = subprocess.run(
                ["ps", "eww", "-o", "command=", "-p", str(pid)],
                capture_output=True,
                text=True,
                timeout=1,
            ).stdout.split()
        except Exception:
            return {}
    return dict(
        entry.split("=", 1)
        for entry in entries
        if entry.startswith("HERDR_") and "=" in entry
    )


def parking_pane(job_dir):
    # A background job runs under the shared Claude daemon, whose environment
    # is that of whichever pane started it; the job is shown in the pane of
    # the interactive session that parked it.
    if not job_dir:
        return None
    job_dir = os.path.normpath(job_dir)
    job_id = os.path.basename(job_dir)
    sessions_dir = os.path.join(os.path.dirname(os.path.dirname(job_dir)), "sessions")
    try:
        names = os.listdir(sessions_dir)
    except OSError:
        return None
    for name in names:
        if not name.endswith(".json"):
            continue
        try:
            with open(os.path.join(sessions_dir, name), encoding="utf-8") as handle:
                record = json.load(handle)
        except Exception:
            continue
        if record.get("kind") != "interactive" or record.get("parkedJobId") != job_id:
            continue
        env = herdr_env(record.get("pid"))
        if env.get("HERDR_ENV") == "1" and env.get("HERDR_PANE_ID") and env.get("HERDR_SOCKET_PATH"):
            return env["HERDR_PANE_ID"], env["HERDR_SOCKET_PATH"]
    return None


if os.environ.get("CLAUDE_CODE_SESSION_KIND") == "bg":
    pane_id, socket_path = parking_pane(os.environ.get("CLAUDE_JOB_DIR")) or (None, None)
if not pane_id or not socket_path:
    raise SystemExit(0)
request_id = f"{source}:{int(time.time() * 1000)}:{random.randrange(1_000_000):06d}"
report_seq = time.time_ns()
session_id = hook_input.get("session_id")
agent_session_id = session_id if isinstance(session_id, str) and session_id else None
transcript_path = hook_input.get("transcript_path")
agent_session_path = transcript_path if isinstance(transcript_path, str) and transcript_path else None
session_start_source = hook_input.get("source") if hook_event_name == "SessionStart" else None
if not isinstance(session_start_source, str) or not session_start_source:
    session_start_source = None
if agent_session_id:
    params = {
        "pane_id": pane_id,
        "source": source,
        "agent": "claude",
        "seq": report_seq,
        "agent_session_id": agent_session_id,
    }
    if agent_session_path:
        params["agent_session_path"] = agent_session_path
    if session_start_source:
        params["session_start_source"] = session_start_source
    request = {
        "id": request_id,
        "method": "pane.report_agent_session",
        "params": params,
    }
else:
    raise SystemExit(0)

try:
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(0.5)
    client.connect(socket_path)
    client.sendall((json.dumps(request) + "\n").encode())
    try:
        client.recv(4096)
    except Exception:
        pass
    client.close()
except Exception:
    pass
PY
