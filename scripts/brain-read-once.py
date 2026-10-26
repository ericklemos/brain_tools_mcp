#!/usr/bin/env python3
"""Claude Code hook: blocks reading the same file range twice in a session.

The agent re-reads files it already has in context: after `brain_search` returned
the verbatim snippet, or after a `Read` earlier in the same session. The second
read costs tokens and adds nothing — the content is already there.

This hook is deliberately narrow. It never blocks the *first* read of a file:
that read is what unlocks `Edit`, so blocking it would deadlock editing. It only
blocks a call whose requested range is already covered by this session's earlier
read of the same, unchanged file. If the file changed on disk (an `Edit`, a
build), the record is stale and the read goes through.

Wire it to both PreToolUse and PostToolUse with matcher "Read"; the event name in
the payload decides whether to record or to guard. See docs/hooks.md.
"""

import json
import os
import sys

STATE_DIR = os.environ.get(
    "BRAIN_READ_ONCE_STATE_DIR", os.path.expanduser("~/.claude/state")
)
DEFAULT_LIMIT = 2000


def state_path(session_id: str) -> str:
    safe = "".join(c if c.isalnum() or c in "-_" else "_" for c in session_id)
    return os.path.join(STATE_DIR, f"read-once-{safe}.json")


def load(session_id: str) -> dict:
    try:
        with open(state_path(session_id), encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return {}


def save(session_id: str, state: dict) -> None:
    try:
        os.makedirs(STATE_DIR, exist_ok=True)
        path = state_path(session_id)
        tmp = path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(state, f)
        os.replace(tmp, path)
    except Exception:
        pass


def requested_range(tool_input: dict) -> tuple:
    offset = tool_input.get("offset") or 0
    limit = tool_input.get("limit") or DEFAULT_LIMIT
    return offset + 1, offset + limit


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except Exception:
        return 0

    if payload.get("tool_name") != "Read":
        return 0

    tool_input = payload.get("tool_input") or {}
    file_path = tool_input.get("file_path")
    if not file_path:
        return 0

    session_id = payload.get("session_id") or "default"
    file_path = os.path.abspath(file_path)

    try:
        mtime = os.stat(file_path).st_mtime_ns
    except OSError:
        return 0  # missing file: let Read report it

    state = load(session_id)
    entry = state.get(file_path)
    # A file that changed since its last read must stay readable.
    fresh = entry if entry and entry.get("mtime") == mtime else None

    if payload.get("hook_event_name") == "PostToolUse":
        lo, hi = requested_range(tool_input)
        ranges = list(fresh["ranges"]) if fresh else []
        ranges.append([lo, hi])
        state[file_path] = {"mtime": mtime, "ranges": ranges}
        save(session_id, state)
        return 0

    if payload.get("hook_event_name") != "PreToolUse" or not fresh:
        return 0

    lo, hi = requested_range(tool_input)
    for a, b in fresh["ranges"]:
        if a <= lo and hi <= b:
            print(
                f"{file_path}: linhas {lo}-{hi} já foram lidas nesta sessão e o arquivo "
                f"não mudou desde então. O conteúdo está no seu contexto — não repita a "
                f"leitura. Se precisar de outra faixa, chame Read com offset fora de "
                f"{a}-{b}.",
                file=sys.stderr,
            )
            return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
