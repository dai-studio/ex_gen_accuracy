#!/usr/bin/env python3
"""
Write outputs/<variant>/stats.md with the token usage and cost of that
variant's Claude Code session.

    ./stats.py <variant>

run.sh calls this automatically when a session exits. Run it by hand to
refresh the numbers after resuming a session.

The session id comes from outputs/<variant>/.session_id (written by run.sh).
Totals come from the transcript's last "cost-state" record, the same numbers
/cost shows. If that record is missing, tokens are summed from the assistant
messages (main session plus subagents) and cost is reported as unknown.
"""

import glob
import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent
VARIANTS = {"prompt_md": "prompt.md", "spec_md": "spec.md", "spec_dai": "spec.dai"}
CONFIG_DIR = Path(os.environ.get("CLAUDE_CONFIG_DIR", Path.home() / ".claude"))

TOKEN_FIELDS = [
    ("inputTokens", "Input"),
    ("outputTokens", "Output"),
    ("thinkingTokens", "Thinking (in output)"),
    ("cacheReadInputTokens", "Cache read"),
    ("cacheCreationInputTokens", "Cache write"),
]


def read_jsonl(path):
    with open(path) as f:
        for line in f:
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


def find_transcript(session_id):
    matches = glob.glob(str(CONFIG_DIR / "projects" / "*" / f"{session_id}.jsonl"))
    if not matches:
        sys.exit(f"no transcript found for session {session_id} under {CONFIG_DIR}/projects")
    return Path(matches[0])


def usage_from_messages(transcript):
    """Fallback: sum usage over unique assistant messages, subagents included."""
    files = [transcript] + sorted(transcript.with_suffix("").glob("subagents/*.jsonl"))
    seen, per_model = set(), {}
    for path in files:
        for rec in read_jsonl(path):
            msg = rec.get("message")
            if rec.get("type") != "assistant" or not isinstance(msg, dict) or "usage" not in msg:
                continue
            if msg.get("id") in seen:
                continue
            seen.add(msg.get("id"))
            u = msg["usage"]
            m = per_model.setdefault(msg.get("model", "unknown"), {k: 0 for k, _ in TOKEN_FIELDS})
            m["inputTokens"] += u.get("input_tokens", 0)
            m["outputTokens"] += u.get("output_tokens", 0)
            m["thinkingTokens"] += (u.get("output_tokens_details") or {}).get("thinking_tokens", 0)
            m["cacheReadInputTokens"] += u.get("cache_read_input_tokens", 0)
            m["cacheCreationInputTokens"] += u.get("cache_creation_input_tokens", 0)
    return per_model


def fmt_duration(ms):
    if ms is None:
        return "n/a"
    s = int(ms // 1000)
    return f"{s // 3600}h {s % 3600 // 60:02d}m {s % 60:02d}s"


def main():
    if len(sys.argv) != 2 or sys.argv[1] not in VARIANTS:
        sys.exit(f"usage: {sys.argv[0]} <{'|'.join(VARIANTS)}>")
    variant = sys.argv[1]
    out_dir = ROOT / "outputs" / variant

    sid_file = out_dir / ".session_id"
    if not sid_file.exists():
        sys.exit(f"{sid_file} not found; start the run with ./run.sh {variant}")
    session_id = sid_file.read_text().strip()
    transcript = find_transcript(session_id)

    records = list(read_jsonl(transcript))
    cost_state = next((r for r in reversed(records) if r.get("type") == "cost-state"), None)
    assistants = [r for r in records if r.get("type") == "assistant"]
    timestamps = [r["timestamp"] for r in records if r.get("timestamp")]

    if cost_state and cost_state.get("modelUsage"):
        per_model = cost_state["modelUsage"]
        source = "`cost-state` record (same totals as `/cost`)"
    else:
        per_model = usage_from_messages(transcript)
        source = "summed from assistant messages; cost unavailable"

    tool_calls = sum(
        1
        for r in assistants
        for block in (r.get("message") or {}).get("content") or []
        if isinstance(block, dict) and block.get("type") == "tool_use"
    )
    first = next((r for r in assistants if r.get("requestedModel") or r.get("effort")), {})
    cs = cost_state or {}

    lines = [
        f"# Run stats: `{variant}`",
        "",
        f"Generated {datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M UTC')} by `stats.py`.",
        "",
        "## Session",
        "",
        "| | |",
        "|---|---|",
        f"| Input file | `{VARIANTS[variant]}` |",
        f"| Session id | `{session_id}` |",
        f"| Model | `{first.get('requestedModel', 'n/a')}` |",
        f"| Effort | `{first.get('effort', 'n/a')}` |",
        f"| Claude Code version | `{first.get('version', 'n/a')}` |",
        f"| Started | {min(timestamps) if timestamps else 'n/a'} |",
        f"| Last activity | {max(timestamps) if timestamps else 'n/a'} |",
        f"| Wall-clock duration | {fmt_duration(cs.get('totalDuration'))} |",
        f"| API duration | {fmt_duration(cs.get('totalAPIDuration'))} |",
        f"| Tool duration | {fmt_duration(cs.get('totalToolDuration'))} |",
        f"| Assistant messages (main session) | {len(assistants)} |",
        f"| Tool calls (main session) | {tool_calls} |",
        f"| Lines added / removed | {cs.get('totalLinesAdded', 'n/a')} / {cs.get('totalLinesRemoved', 'n/a')} |",
        "",
        "## Tokens and cost by model",
        "",
        f"Source: {source}. Subagent and background-model usage is included.",
        "",
        "| Model | " + " | ".join(label for _, label in TOKEN_FIELDS) + " | Web searches | Cost (USD) |",
        "|---|" + "---:|" * (len(TOKEN_FIELDS) + 2),
    ]

    totals = {k: 0 for k, _ in TOKEN_FIELDS}
    total_searches = 0
    for model, u in sorted(per_model.items()):
        for k, _ in TOKEN_FIELDS:
            totals[k] += u.get(k, 0)
        total_searches += u.get("webSearchRequests", 0)
        cost = f"${u['costUSD']:.4f}" if "costUSD" in u else "n/a"
        lines.append(
            f"| `{model}` | "
            + " | ".join(f"{u.get(k, 0):,}" for k, _ in TOKEN_FIELDS)
            + f" | {u.get('webSearchRequests', 0)} | {cost} |"
        )
    total_cost = f"**${cs['totalCostUSD']:.4f}**" if "totalCostUSD" in cs else "**n/a**"
    lines.append(
        "| **Total** | "
        + " | ".join(f"**{totals[k]:,}**" for k, _ in TOKEN_FIELDS)
        + f" | **{total_searches}** | {total_cost} |"
    )

    all_input = totals["inputTokens"] + totals["cacheReadInputTokens"] + totals["cacheCreationInputTokens"]
    lines += [
        "",
        f"Total input incl. cache: **{all_input:,}** tokens. "
        f"Total output: **{totals['outputTokens']:,}** tokens.",
    ]
    if cs.get("hasUnknownModelCost"):
        lines += ["", "> Cost is incomplete: at least one model had no known price."]

    (out_dir / "stats.md").write_text("\n".join(lines) + "\n")
    print(f"wrote {out_dir / 'stats.md'}")


if __name__ == "__main__":
    main()
