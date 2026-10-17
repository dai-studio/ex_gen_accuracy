#!/usr/bin/env bash
#
# Start a clean Claude Code session that builds Moxy from ONE input format.
#
#   ./run.sh <variant>
#
#   variant:  prompt_md   -> outputs/prompt_md/prompt.md   (one-paragraph prompt)
#             spec_md     -> outputs/spec_md/spec.md       (prose specification)
#             spec_dai    -> outputs/spec_dai/spec.dai     (BPMN + dai: structured spec)
#
# Optional environment overrides (keep them identical across all three runs):
#   MODEL=opus            model alias or full id
#   EFFORT=high           low | medium | high | xhigh | max
#   PERMISSION_MODE=acceptEdits   permission mode for the session
#
# "Clean" means: --safe-mode (no CLAUDE.md, memory, skills, plugins, hooks,
# MCP servers, custom agents or output styles), no Chrome integration, and
# deny rules that stop the session reading the original inputs/ or the other
# variants' folders. Every variant gets the same kickoff prompt; only the
# input filename differs.
#
# When the session exits, stats.py writes outputs/<variant>/stats.md with the
# run's token usage and cost.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VARIANTS=(prompt_md spec_md spec_dai)

usage() {
  echo "usage: $0 <prompt_md|spec_md|spec_dai>" >&2
  exit 1
}

[[ $# -eq 1 ]] || usage
VARIANT="$1"

case "$VARIANT" in
  prompt_md) INPUT_FILE="prompt.md" ;;
  spec_md)   INPUT_FILE="spec.md" ;;
  spec_dai)  INPUT_FILE="spec.dai" ;;
  *) usage ;;
esac

WORKDIR="$ROOT/outputs/$VARIANT"
[[ -f "$WORKDIR/$INPUT_FILE" ]] || { echo "missing $WORKDIR/$INPUT_FILE" >&2; exit 1; }

command -v claude >/dev/null || { echo "claude CLI not found on PATH" >&2; exit 1; }

MODEL="${MODEL:-opus}"
EFFORT="${EFFORT:-high}"
PERMISSION_MODE="${PERMISSION_MODE:-acceptEdits}"

# Deny file-tool access to the source inputs and to every other variant.
# Absolute paths in permission rules start with "//".
deny_rules=("\"Read(/$ROOT/inputs/**)\"")
for v in "${VARIANTS[@]}"; do
  [[ "$v" == "$VARIANT" ]] && continue
  deny_rules+=("\"Read(/$ROOT/outputs/$v/**)\"" "\"Edit(/$ROOT/outputs/$v/**)\"")
done
SETTINGS="{\"permissions\":{\"deny\":[$(IFS=,; echo "${deny_rules[*]}")]}}"

SYSTEM_APPEND="You are taking part in a controlled experiment. Your only source of \
requirements is the file ./$INPUT_FILE in the current working directory. Do not \
read, list or search any file outside the current working directory, and do not \
look for other specifications. Put all code you write inside the current \
working directory."

KICKOFF="Build the project described in ./$INPUT_FILE. Implement it completely \
in this directory, make it build, and add tests where they make sense. When you \
finish, write NOTES.md summarising what you built, any decisions you had to \
make yourself, and anything you left unimplemented."

# Pin the session id so stats.py can find this run's transcript afterwards.
SESSION_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
echo "$SESSION_ID" > "$WORKDIR/.session_id"

cd "$WORKDIR"

set +e
claude \
  --safe-mode \
  --no-chrome \
  --strict-mcp-config \
  --model "$MODEL" \
  --effort "$EFFORT" \
  --permission-mode "$PERMISSION_MODE" \
  --settings "$SETTINGS" \
  --append-system-prompt "$SYSTEM_APPEND" \
  --name "moxy-$VARIANT" \
  --session-id "$SESSION_ID" \
  "$KICKOFF"
STATUS=$?
set -e

# Record tokens and cost in outputs/<variant>/stats.md.
python3 "$ROOT/stats.py" "$VARIANT" || echo "stats.py failed; rerun ./stats.py $VARIANT" >&2

exit "$STATUS"
