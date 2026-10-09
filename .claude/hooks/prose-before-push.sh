#!/bin/sh
input=$(cat)
command=$(printf '%s' "$input" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("tool_input",{}).get("command",""))')
case "$command" in
  *"git push"*) ;;
  *) exit 0 ;;
esac
root=$(git rev-parse --show-toplevel)
if ! report=$(python3 "$root/scripts/check-prose.py" 2>&1); then
  printf '%s\n\nFix every finding above before pushing: rewrite the sentence in plain words (follow .claude/skills/writing-blogs/SKILL.md), commit the fix, then push again.\n' "$report" >&2
  exit 2
fi
