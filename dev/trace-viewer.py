#!/usr/bin/env python3
"""Mentat runtime trace log viewer.

Searches, browses, and steps through JSONL runtime trace events grouped
by turn_id into sessions.  Designed for fast tail-reading of large files.
"""

import argparse
import json
import os
import sys

# ---------------------------------------------------------------------------
# Color helpers
# ---------------------------------------------------------------------------

COLORS = {
    "cyan": "\033[36m",
    "green": "\033[32m",
    "blue": "\033[34m",
    "yellow": "\033[33m",
    "red": "\033[31m",
    "dim": "\033[2m",
    "bold": "\033[1m",
    "reset": "\033[0m",
}

_use_color = True


def _init_color(force_no_color: bool) -> None:
    global _use_color
    _use_color = (not force_no_color) and sys.stdout.isatty()


def c(text: str, *styles: str) -> str:
    if not _use_color:
        return text
    prefix = "".join(COLORS.get(s, "") for s in styles)
    return f"{prefix}{text}{COLORS['reset']}" if prefix else text


# ---------------------------------------------------------------------------
# JSONL line parser
# ---------------------------------------------------------------------------

def parse_line(line: str) -> dict | None:
    line = line.strip()
    if not line:
        return None
    try:
        return json.loads(line)
    except (json.JSONDecodeError, ValueError):
        return None


# ---------------------------------------------------------------------------
# File readers
# ---------------------------------------------------------------------------

CHUNK_SIZE = 8192


def reverse_line_reader(path: str):
    """Yield lines from *path* in reverse order without loading the whole file."""
    with open(path, "rb") as fh:
        fh.seek(0, os.SEEK_END)
        remaining = fh.tell()
        buf = b""
        while remaining > 0:
            read_size = min(CHUNK_SIZE, remaining)
            remaining -= read_size
            fh.seek(remaining)
            chunk = fh.read(read_size)
            buf = chunk + buf
            lines = buf.split(b"\n")
            # first element may be partial — keep it for next iteration
            buf = lines[0]
            for raw in reversed(lines[1:]):
                yield raw.decode("utf-8", errors="replace")
        if buf:
            yield buf.decode("utf-8", errors="replace")


def forward_line_reader(path: str):
    """Yield parsed event dicts line-by-line from the start of *path*."""
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            evt = parse_line(line)
            if evt is not None:
                yield evt


# ---------------------------------------------------------------------------
# Session grouping
# ---------------------------------------------------------------------------

class Session:
    __slots__ = (
        "turn_id", "events", "start_time", "end_time",
        "channel", "model", "user_prompt",
    )

    def __init__(self, turn_id: str):
        self.turn_id = turn_id
        self.events: list[dict] = []
        self.start_time: str = ""
        self.end_time: str = ""
        self.channel: str = ""
        self.model: str = ""
        self.user_prompt: str = ""

    def finalize(self) -> None:
        self.events.sort(key=lambda e: e.get("timestamp", ""))
        if self.events:
            self.start_time = self.events[0].get("timestamp", "")
            self.end_time = self.events[-1].get("timestamp", "")
        for ev in self.events:
            if not self.channel and ev.get("channel"):
                self.channel = ev["channel"]
            if not self.model and ev.get("model"):
                self.model = ev["model"]
            if not self.user_prompt and ev.get("event_type") == "channel_message_inbound":
                payload = ev.get("payload") or {}
                self.user_prompt = payload.get("content_preview", "")


def group_events_into_sessions(events: list[dict]) -> list[Session]:
    """Group events by turn_id, associating orphan inbound/outbound events
    with the nearest session.

    In the real trace format, ``channel_message_inbound`` and
    ``channel_message_outbound`` carry no ``turn_id``.  An inbound event is
    the *trigger* for the next turn_id that appears, and an outbound event is
    the *delivery* of the preceding turn_id's response.  We attach them
    accordingly via a two-pass positional association.
    """
    buckets: dict[str, Session] = {}
    orphans_before: list[tuple[int, dict]] = []  # (index, event)
    last_tid: str | None = None

    # Pass 1: bucket events with a turn_id; track orphans.
    for i, ev in enumerate(events):
        tid = ev.get("turn_id")
        if tid:
            if tid not in buckets:
                buckets[tid] = Session(tid)
            buckets[tid].events.append(ev)
            # Flush any preceding orphans into this session (inbound triggers)
            for _, orphan in orphans_before:
                buckets[tid].events.append(orphan)
            orphans_before.clear()
            last_tid = tid
        else:
            etype = ev.get("event_type", "")
            if etype == "channel_message_outbound" and last_tid and last_tid in buckets:
                # Outbound belongs to the session that just finished
                buckets[last_tid].events.append(ev)
            elif etype == "channel_message_inbound":
                # Inbound triggers the *next* session — hold until we see it
                orphans_before.append((i, ev))
            else:
                orphans_before.append((i, ev))

    # Any remaining orphans that never got a following turn_id
    standalone: list[Session] = []
    for _, ev in orphans_before:
        s = Session("(no turn_id)")
        s.events.append(ev)
        s.finalize()
        standalone.append(s)

    sessions = list(buckets.values())
    for s in sessions:
        s.finalize()
    sessions.extend(standalone)
    sessions.sort(key=lambda s: s.start_time, reverse=True)
    return sessions


# ---------------------------------------------------------------------------
# Search
# ---------------------------------------------------------------------------

def search_sessions(path: str, term: str) -> list[Session]:
    """Find sessions whose inbound prompt matches *term*.

    Because ``channel_message_inbound`` carries no ``turn_id`` in the real
    trace format, we do positional association: an inbound event is linked
    to the next ``turn_id`` that appears in the stream.
    """
    term_lower = term.lower()
    matching_turn_ids: set[str] = set()

    # Pass 1: find turn_ids whose preceding inbound prompt matches.
    pending_match = False
    for evt in forward_line_reader(path):
        etype = evt.get("event_type", "")
        tid = evt.get("turn_id")
        if etype == "channel_message_inbound":
            payload = evt.get("payload") or {}
            preview = payload.get("content_preview", "")
            pending_match = term_lower in preview.lower()
            # Also handle the case where inbound *does* carry a turn_id
            if pending_match and tid:
                matching_turn_ids.add(tid)
                pending_match = False
        elif tid and pending_match:
            matching_turn_ids.add(tid)
            pending_match = False

    if not matching_turn_ids:
        return []

    # Pass 2: collect all events that belong to matched sessions
    # (including surrounding orphan inbound/outbound via group_events)
    matching_events: list[dict] = []
    pending_orphans: list[dict] = []
    last_matched = False
    for evt in forward_line_reader(path):
        tid = evt.get("turn_id")
        if tid:
            is_match = tid in matching_turn_ids
            if is_match:
                matching_events.extend(pending_orphans)
                matching_events.append(evt)
            pending_orphans.clear()
            last_matched = is_match
        else:
            etype = evt.get("event_type", "")
            if etype == "channel_message_outbound" and last_matched:
                matching_events.append(evt)
            else:
                pending_orphans.append(evt)

    return group_events_into_sessions(matching_events)


# ---------------------------------------------------------------------------
# Recent sessions via reverse reader
# ---------------------------------------------------------------------------

def recent_sessions(path: str, count: int) -> list[Session]:
    events: list[dict] = []
    seen_turn_ids: set[str] = set()

    for line in reverse_line_reader(path):
        evt = parse_line(line)
        if evt is None:
            continue
        tid = evt.get("turn_id")
        if tid and tid not in seen_turn_ids:
            seen_turn_ids.add(tid)
        events.append(evt)
        # Once we've seen enough distinct turn_ids, keep reading until we
        # finish the oldest session boundary (its first event).  A simple
        # heuristic: after collecting count turn_ids, stop when we hit
        # another new one (meaning we've passed the boundary).
        if len(seen_turn_ids) > count:
            break

    events.reverse()
    sessions = group_events_into_sessions(events)
    return sessions[:count]


# ---------------------------------------------------------------------------
# Session list display
# ---------------------------------------------------------------------------

def display_session_list(sessions: list[Session], search_term: str | None = None) -> None:
    if not sessions:
        if search_term:
            print(f'No sessions matching "{search_term}".')
        else:
            print("No sessions found.")
        return

    header = (
        f'Sessions matching "{search_term}"'
        if search_term
        else "Recent sessions"
    )
    print(c(header, "cyan", "bold"))
    print()
    for i, s in enumerate(sessions, 1):
        ts = s.start_time or "unknown"
        ch = s.channel or "?"
        mdl = s.model or "?"
        n_events = len(s.events)
        idx = c(f"[{i}]", "bold")
        meta = c(f"{ts} | {ch} | {mdl} | {n_events} events", "dim")
        print(f"{idx} {meta}")
        prompt = s.user_prompt or "(no user prompt)"
        if len(prompt) > 100:
            prompt = prompt[:97] + "..."
        print(f"    {c(prompt, 'green')}")


# ---------------------------------------------------------------------------
# Event-type renderers
# ---------------------------------------------------------------------------

def _render_inbound(payload: dict) -> list[str]:
    lines = []
    sender = payload.get("sender", "?")
    content = payload.get("content_preview", "")
    lines.append(f"  {c('Sender:', 'bold')} {sender}")
    lines.append(f"  {c('Content:', 'bold')} {c(content, 'green')}")
    return lines


def _render_outbound(payload: dict) -> list[str]:
    preview = ""
    for k in ("content_preview", "content", "text"):
        if k in payload:
            preview = str(payload[k])
            break
    if not preview:
        preview = str(payload) if payload else "(empty)"
    if len(preview) > 300:
        preview = preview[:297] + "..."
    return [f"  {c('Content:', 'bold')} {c(preview, 'blue')}"]


def _render_llm_request(payload: dict, event: dict | None = None) -> list[str]:
    lines = []
    model = (event or {}).get("model") or payload.get("model")
    if model:
        lines.append(f"  {c('Model:', 'bold')} {c(model, 'dim')}")
    lines.append(f"  {c('Iteration:', 'bold')} {payload.get('iteration', '?')}")
    lines.append(f"  {c('Messages:', 'bold')} {payload.get('messages_count', '?')}")
    prompt_arr = payload.get("prompt") or []
    if not prompt_arr:
        return lines
    last_msg = prompt_arr[-1]
    if not isinstance(last_msg, dict):
        return lines
    role = last_msg.get("role", "?")
    content = str(last_msg.get("content", ""))
    # On the first iteration the last message is typically the user prompt;
    # on subsequent iterations it's a tool result.  Show the role and full
    # content so the developer can see exactly what was sent to the LLM.
    label = "System Prompt" if role == "system" else f"Last Message ({role})"
    lines.append(f"  {c(f'{label}:', 'bold')}")
    for line in content.split("\n"):
        lines.append(f"    {c(line, 'dim')}")
    return lines


def _render_llm_response(payload: dict, event: dict | None = None) -> list[str]:
    lines = []
    model = (event or {}).get("model") or payload.get("model")
    if model:
        lines.append(f"  {c('Model:', 'bold')} {c(model, 'dim')}")
    lines.append(f"  {c('Iteration:', 'bold')} {payload.get('iteration', '?')}")
    dur = payload.get("duration_ms")
    if dur is not None:
        lines.append(f"  {c('Duration:', 'bold')} {c(f'{dur}ms', 'dim')}")
    inp = payload.get("input_tokens", "?")
    out = payload.get("output_tokens", "?")
    cached = payload.get("cached_input_tokens", 0)
    lines.append(f"  {c('Tokens:', 'bold')} {c(f'{inp} in / {out} out / {cached} cached', 'dim')}")
    cost = payload.get("cost_usd")
    if cost is not None:
        lines.append(f"  {c('Cost:', 'bold')} {c(f'${cost}', 'dim')}")
    return lines


def _render_tool_call_start(payload: dict) -> list[str]:
    lines = []
    tool = payload.get("tool", "?")
    lines.append(f"  {c('Tool:', 'bold')} {c(tool, 'yellow')}")
    args = payload.get("arguments")
    if args is not None:
        try:
            pretty = json.dumps(args, indent=2)
        except (TypeError, ValueError):
            pretty = str(args)
        for line in pretty.split("\n"):
            lines.append(f"  {c(line, 'dim')}")
    return lines


def _render_tool_call_result(payload: dict, event: dict | None = None) -> list[str]:
    lines = []
    tool = payload.get("tool", "?")
    lines.append(f"  {c('Tool:', 'bold')} {c(tool, 'yellow')}")
    if event is not None:
        success = event.get("success")
        if success is True:
            lines.append(f"  {c('Status:', 'bold')} {c('OK', 'green')}")
        elif success is False:
            lines.append(f"  {c('Status:', 'bold')} {c('FAILED', 'red')}")
        model = event.get("model")
        if model:
            lines.append(f"  {c('Model:', 'bold')} {c(model, 'dim')}")
        msg = event.get("message")
        if msg:
            lines.append(f"  {c('Message:', 'bold')} {msg}")
    dur = payload.get("duration_ms")
    if dur is not None:
        lines.append(f"  {c('Duration:', 'bold')} {c(f'{dur}ms', 'dim')}")
    output = payload.get("output", "")
    out_str = str(output)
    if len(out_str) > 500:
        out_str = out_str[:497] + "..."
    lines.append(f"  {c('Output:', 'bold')} {out_str}")
    # Show extra payload keys beyond the standard ones
    extra_keys = set(payload.keys()) - {"tool", "duration_ms", "output", "iteration", "deduplicated"}
    if extra_keys:
        for k in sorted(extra_keys):
            val = payload[k]
            val_str = str(val)
            if len(val_str) > 300:
                val_str = val_str[:297] + "..."
            lines.append(f"  {c(f'{k}:', 'bold')} {c(val_str, 'dim')}")
    return lines


def _render_turn_final(payload: dict) -> list[str]:
    lines = []
    lines.append(f"  {c('Iteration:', 'bold')} {payload.get('iteration', '?')}")
    text = payload.get("text", "")
    if len(text) > 500:
        text = text[:497] + "..."
    lines.append(f"  {c('Text:', 'bold')} {c(text, 'blue')}")
    return lines


def _render_provider_api_request(payload: dict, event: dict | None = None) -> list[str]:
    """Render the actual API request payload sent to a provider."""
    lines = []
    model = payload.get("model", "?")
    provider = (event or {}).get("provider", "?")
    lines.append(f"  {c('Provider:', 'bold')} {c(provider, 'dim')}")
    lines.append(f"  {c('Model:', 'bold')} {c(model, 'dim')}")
    stream = payload.get("stream", False)
    lines.append(f"  {c('Stream:', 'bold')} {c(str(stream), 'dim')}")

    # Options (temperature, num_ctx, etc.)
    options = payload.get("options") or {}
    if options:
        parts = []
        for k, v in options.items():
            if v is not None:
                parts.append(f"{k}={v}")
        lines.append(f"  {c('Options:', 'bold')} {c(', '.join(parts), 'dim')}")

    # Tools summary
    tools = payload.get("tools") or []
    if tools:
        names = [t.get("function", {}).get("name", "?") for t in tools]
        lines.append(f"  {c(f'Tools ({len(names)}):', 'bold')} {c(', '.join(names), 'dim')}")
    else:
        lines.append(f"  {c('Tools:', 'bold')} {c('none (prompt-guided)', 'dim')}")

    # Think mode
    think = payload.get("think")
    if think is not None:
        lines.append(f"  {c('Think:', 'bold')} {c(str(think), 'dim')}")

    # Messages summary
    messages = payload.get("messages") or []
    lines.append(f"  {c('Messages:', 'bold')} {len(messages)}")
    for msg in messages:
        role = msg.get("role", "?")
        content = msg.get("content", "")
        has_images = bool(msg.get("images"))
        has_tool_calls = bool(msg.get("tool_calls"))
        suffix = ""
        if has_images:
            suffix += " [+images]"
        if has_tool_calls:
            suffix += " [+tool_calls]"
        preview = content[:120].replace("\n", "\\n")
        if len(content) > 120:
            preview += "..."
        lines.append(f"    {c(role, 'bold')}: {c(preview, 'dim')}{c(suffix, 'yellow')}")

    return lines


def _render_generic(payload: dict) -> list[str]:
    if not payload:
        return ["  (empty payload)"]
    lines = [f"  {c('Payload keys:', 'bold')} {', '.join(str(k) for k in payload.keys())}"]
    try:
        pretty = json.dumps(payload, indent=2, default=str)
        for line in pretty.split("\n")[:20]:
            lines.append(f"  {c(line, 'dim')}")
        if pretty.count("\n") > 20:
            lines.append(f"  {c('... (truncated)', 'dim')}")
    except (TypeError, ValueError):
        lines.append(f"  {str(payload)[:300]}")
    return lines


_RENDERERS = {
    "channel_message_inbound": _render_inbound,
    "channel_message_outbound": _render_outbound,
    "llm_request": _render_llm_request,
    "llm_response": _render_llm_response,
    "tool_call_start": _render_tool_call_start,
    "tool_call_result": _render_tool_call_result,
    "turn_final_response": _render_turn_final,
    "provider_api_request": _render_provider_api_request,
}


# ---------------------------------------------------------------------------
# Turn formatter
# ---------------------------------------------------------------------------

def format_turn(index: int, total: int, event: dict) -> str:
    etype = event.get("event_type", "unknown")
    ts = event.get("timestamp", "?")

    # Color the event type label by category
    if "error" in etype or "timeout" in etype or "cancelled" in etype:
        etype_colored = c(etype, "red")
    elif etype.startswith("tool_call"):
        etype_colored = c(etype, "yellow")
    elif etype == "channel_message_inbound":
        etype_colored = c(etype, "green")
    elif etype in ("channel_message_outbound", "turn_final_response"):
        etype_colored = c(etype, "blue")
    else:
        etype_colored = etype

    event_id = event.get("id", "")
    id_suffix = f" ── {c(event_id, 'dim')}" if event_id else ""
    turn_id = event.get("turn_id", "")
    tid_suffix = f" ── turn_id={c(turn_id, 'dim')}" if turn_id else ""
    header = c(f"── Turn {index}/{total} ── ", "cyan") + etype_colored + c(f" ── {ts} ──", "cyan") + tid_suffix + id_suffix
    payload = event.get("payload") or {}
    renderer = _RENDERERS.get(etype, _render_generic)
    if etype in ("tool_call_result", "llm_request", "llm_response", "provider_api_request"):
        body_lines = renderer(payload, event=event)
    else:
        body_lines = renderer(payload)
    return header + "\n" + "\n".join(body_lines)


# ---------------------------------------------------------------------------
# Interactive stepper
# ---------------------------------------------------------------------------

def _find_system_prompt(session: Session) -> str | None:
    """Return the system prompt from the first llm_request in the session, if any."""
    for ev in session.events:
        if ev.get("event_type") != "llm_request":
            continue
        prompt_arr = (ev.get("payload") or {}).get("prompt") or []
        if not prompt_arr:
            continue
        first = prompt_arr[0]
        if isinstance(first, dict) and first.get("role") == "system":
            return str(first.get("content", ""))
    return None


def _display_system_prompt(session: Session) -> None:
    text = _find_system_prompt(session)
    if text is None:
        print(c("  (no system prompt found in this session)", "dim"))
        return
    print()
    print(c("── System Prompt ──", "cyan", "bold"))
    for line in text.split("\n"):
        print(f"  {c(line, 'dim')}")
    print(c("── End System Prompt ──", "cyan", "bold"))


def _find_provider_api_request(session: Session) -> dict | None:
    """Return the first provider_api_request event payload in the session."""
    for ev in session.events:
        if ev.get("event_type") == "provider_api_request":
            return ev.get("payload")
    return None


def _display_provider_api_request(session: Session) -> None:
    payload = _find_provider_api_request(session)
    if payload is None:
        print(c("  (no provider API request in this session)", "dim"))
        return
    print()
    print(c("── Provider API Request (full payload) ──", "cyan", "bold"))
    # Show the full JSON, but truncate message content for readability
    display = dict(payload)
    messages = display.get("messages") or []
    summarized = []
    for msg in messages:
        m = dict(msg)
        content = m.get("content", "")
        if len(content) > 500:
            m["content"] = content[:500] + f"... ({len(content)} chars total)"
        summarized.append(m)
    display["messages"] = summarized
    print(json.dumps(display, indent=2, default=str))
    print(c("── End Provider API Request ──", "cyan", "bold"))


def stepper(session: Session, dump: bool = False) -> None:
    events = session.events
    total = len(events)
    if total == 0:
        print("  (no events in this session)")
        return

    has_system_prompt = _find_system_prompt(session) is not None
    has_provider_request = _find_provider_api_request(session) is not None

    if dump:
        for i, ev in enumerate(events, 1):
            print(format_turn(i, total, ev))
            print()
        return

    idx = 0
    while True:
        print()
        print(format_turn(idx + 1, total, events[idx]))
        print()

        parts = []
        if idx < total - 1:
            parts.append("Enter/n=next")
        parts.append("p=prev")
        parts.append("q=quit")
        parts.append("#=jump")
        parts.append("r=raw")
        if has_system_prompt:
            parts.append("s=system prompt")
        if has_provider_request:
            parts.append("t=tools/provider")
        prompt_text = c(f"[{', '.join(parts)}] ", "dim")

        try:
            raw = input(prompt_text).strip().lower()
        except (EOFError, KeyboardInterrupt):
            break

        if raw in ("q", "quit"):
            break
        elif raw in ("", "n"):
            if idx < total - 1:
                idx += 1
        elif raw == "p":
            if idx > 0:
                idx -= 1
        elif raw == "r":
            print()
            print(c("── Raw JSON ──", "cyan", "bold"))
            print(json.dumps(events[idx], indent=2, default=str))
            print(c("── End Raw JSON ──", "cyan", "bold"))
        elif raw == "s" and has_system_prompt:
            _display_system_prompt(session)
        elif raw == "t" and has_provider_request:
            _display_provider_api_request(session)
        else:
            try:
                jump = int(raw)
                if 1 <= jump <= total:
                    idx = jump - 1
                else:
                    print(c(f"  Turn number must be 1-{total}", "red"))
            except ValueError:
                pass


# ---------------------------------------------------------------------------
# Session selection prompt
# ---------------------------------------------------------------------------

def session_select_loop(sessions: list[Session], dump: bool = False) -> None:
    while True:
        print()
        display_session_list(sessions)
        if not sessions:
            return
        print()
        try:
            raw = input(c("Select session [1-{}, q=quit]: ".format(len(sessions)), "dim")).strip().lower()
        except (EOFError, KeyboardInterrupt):
            return

        if raw in ("q", "quit", ""):
            return
        try:
            choice = int(raw)
        except ValueError:
            continue
        if 1 <= choice <= len(sessions):
            stepper(sessions[choice - 1], dump=dump)
        else:
            print(c(f"  Enter a number 1-{len(sessions)}", "red"))


# ---------------------------------------------------------------------------
# File validation
# ---------------------------------------------------------------------------

def validate_file(path: str) -> bool:
    if not os.path.isfile(path):
        print(f"Error: file not found: {path}", file=sys.stderr)
        return False
    if not os.access(path, os.R_OK):
        print(f"Error: file not readable: {path}", file=sys.stderr)
        return False
    return True


# ---------------------------------------------------------------------------
# CLI entry point
# ---------------------------------------------------------------------------

def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        description="Browse and search Mentat runtime trace logs.",
    )
    p.add_argument(
        "search_term",
        nargs="?",
        default=None,
        metavar="SEARCH_TERM",
        help="Substring to search for in user prompt content",
    )
    p.add_argument(
        "-f", "--file",
        default="state/runtime-trace.jsonl",
        metavar="PATH",
        help="Path to the JSONL trace log file (default: state/runtime-trace.jsonl)",
    )
    p.add_argument(
        "-n", "--last",
        type=int,
        default=10,
        metavar="N",
        help="Number of recent sessions to show (default: 10)",
    )
    p.add_argument(
        "--no-color",
        action="store_true",
        help="Disable color output",
    )
    p.add_argument(
        "-d", "--dump",
        action="store_true",
        help="Dump all turns without interactive stepping",
    )
    return p


def main() -> None:
    parser = build_parser()
    args = parser.parse_args()

    _init_color(args.no_color)

    if not validate_file(args.file):
        sys.exit(1)

    if os.path.getsize(args.file) == 0:
        print("Trace file is empty.")
        sys.exit(0)

    if args.search_term:
        sessions = search_sessions(args.file, args.search_term)
    else:
        sessions = recent_sessions(args.file, args.last)

    if args.dump and sessions:
        display_session_list(sessions, search_term=args.search_term)
        print()
        for s in sessions:
            print(c(f"=== Session: {s.turn_id} ===", "cyan", "bold"))
            stepper(s, dump=True)
    else:
        session_select_loop(sessions, dump=args.dump)


if __name__ == "__main__":
    main()
