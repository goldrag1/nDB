#!/usr/bin/env bash
# A real Claude agent adds ONE new memory to nDB via the ndblive MCP server.
# Scheduled by ndb-remember.timer. Runs on a machine with claude auth (~/.claude)
# and the ndblive MCP registered (claude mcp add ... ndblive). The MCP is
# tailnet-only, so this must run somewhere on the tailnet (not a cloud routine).
set -u
export PATH="$HOME/.local/bin:$PATH"
LOG="$HOME/.local/state/ndb-remember.log"
mkdir -p "$(dirname "$LOG")"

PROMPT='You have an MCP server "ndblive" = an nDB agent-memory graph: a coding agent remembering this software project. Schema: entity type_ids {1 Person, 2 File, 3 Commit, 4 Issue, 6 Observation}; property ids {12 path, 13 message, 14 title}; hyperedge role ids {2 touches, 6 observation}. Using ONLY the ndblive MCP tools:
1) Call ndb.iter (limit 80) to read current memory, INCLUDING existing Observations whose message starts with "[live-claude]".
2) Identify ONE real, genuinely NEW (non-duplicate) relationship in the File / Commit / Issue data that you have not already recorded.
3) Call ndb.commit_entity type_id 6 with properties [{"prop_id":13,"value":{"tag":"string","value":"[live-claude] <one concise new insight>"}}]; keep the returned entity_id.
4) Call ndb.commit_hyperedge type_id 102 linking your observation (role_id 6 -> the observation entity_id) to 2-3 relevant File entity UUIDs (role_id 2 each).
5) Reply with one line: the insight text and the entity_id.
Make it distinct from prior [live-claude] notes. Do not use any non-ndblive tools.'

echo "=== $(date -Is) ===" >> "$LOG"
timeout 240 claude -p "$PROMPT" --permission-mode bypassPermissions --max-turns 18 >> "$LOG" 2>&1
status=$?
echo "(exit $status)" >> "$LOG"
echo "" >> "$LOG"
exit 0
