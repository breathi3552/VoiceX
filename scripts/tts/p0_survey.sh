#!/usr/bin/env bash
#
# P0 selection-path survey (docs/tts_plan.md §7, step 2).
#
# Phase 0 measured two applications and found Safari falling back to Copy. This
# runs the same method across the whole P0 list so the real path distribution is
# known before phase 1 decides what to build: whether an AX range layer helps
# anybody, and whether "Copy compatibility mode is switchable off" (§4.5) is
# still a defensible default.
#
# For each application: stage a fixture, put keyboard focus in it, Select All,
# inject the read hotkey, and read the answer out of VoiceX's structured log.
#
# This is a survey, not the release gate. It reports what each application does;
# it does not implement §4.3, and it must not be quoted as GO.
#
# Prerequisites (residual manual item #1 in the plan):
#   - VoiceX running with stderr captured:
#       pnpm tauri dev 2>&1 | tee /tmp/voicex-tts-survey.log
#   - The running VoiceX binary has Accessibility + Input Monitoring granted.
#   - The calling terminal has Accessibility and Automation granted.
#
# Usage:
#   scripts/tts/p0_survey.sh [--log PATH] [--app NAME[,NAME…]] [--list]
#
#   --app defaults to every P0 application. Names: textedit safari chrome
#   vscode notes preview terminal.

set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

LOG_FILE="${VOICEX_LOG:-/tmp/voicex-tts-survey.log}"
ALL_APPS="textedit safari chrome vscode notes preview terminal"
TARGET_APPS="$ALL_APPS"

SPEAK_TIMEOUT_S=10
STOP_TIMEOUT_S=5

while [ $# -gt 0 ]; do
  case "$1" in
    --log)  LOG_FILE="$2"; shift 2 ;;
    --app)  TARGET_APPS="$(echo "$2" | tr ',' ' ')"; shift 2 ;;
    --list) echo "$ALL_APPS" | tr ' ' '\n'; exit 0 ;;
    -h|--help) sed -n '2,31p' "$0"; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

# --- fixtures ----------------------------------------------------------------

FIXTURE_TEXT="VoiceX selected text reading survey $RUN_ID. 这是一段用于验证跨应用朗读链路的中文测试文本。The quick brown fox jumps over the lazy dog, and then reads it back aloud."

# cupsfilter's text-to-PDF path has no CJK font, so the Preview fixture stays
# ASCII — otherwise the characters never make it into the document and the
# expected length would be wrong for reasons that have nothing to do with the
# selection reader.
FIXTURE_ASCII="VoiceX selected text reading survey $RUN_ID. The quick brown fox jumps over the lazy dog, and then reads it back aloud."

# Rust logs chars().count(), i.e. Unicode scalars — the unit Python's len()
# gives. Asserting on it is what proves we read the fixture rather than, say,
# the address bar, which a bare "selection succeeded" check accepts happily.
count_chars() { python3 -c 'import sys; print(len(sys.argv[1]))' "$1"; }
CHARS_MIXED="$(count_chars "$FIXTURE_TEXT")"
CHARS_ASCII="$(count_chars "$FIXTURE_ASCII")"

write_html_fixture() {
  cat > "$1" <<HTML
<!doctype html><meta charset="utf-8"><title>VoiceX TTS fixture</title>
<body><p>$FIXTURE_TEXT</p></body>
HTML
}

# --- driver: TextEdit --------------------------------------------------------

APP_textedit_bundle="com.apple.TextEdit"
APP_textedit_process="TextEdit"
APP_textedit_chars="$CHARS_MIXED"
APP_textedit_match="exact"

setup_textedit() {
  open -a TextEdit
  app_ready TextEdit || return 1
  osa 25 >/dev/null <<APPLESCRIPT
tell application "TextEdit"
  activate
  set newDoc to make new document
  set text of newDoc to "$FIXTURE_TEXT"
end tell
APPLESCRIPT
  sleep 1
}

teardown_textedit() {
  # Matched on the run marker, never on position: `close document 1 saving no`
  # discards whatever happens to be frontmost, including the user's unsaved work.
  osa 25 >/dev/null <<APPLESCRIPT || true
tell application "TextEdit"
  repeat with i from (count of documents) to 1 by -1
    try
      if (text of document i) contains "$RUN_ID" then close document i saving no
    end try
  end repeat
end tell
APPLESCRIPT
}

# --- driver: Safari ----------------------------------------------------------

APP_safari_bundle="com.apple.Safari"
APP_safari_process="Safari"
APP_safari_chars="$CHARS_MIXED"
APP_safari_match="exact"

SAFARI_WINDOW_ID=""

setup_safari() {
  local fixture="$TMP_DIR/fixture-$RUN_ID.html"
  write_html_fixture "$fixture"
  open -a Safari
  app_ready Safari || return 1

  # Two steps on purpose. Safari ignores AppleScript navigation to file:// URLs
  # (`set URL of …` leaves the tab on favorites://), so the page has to go
  # through Launch Services — but `open -a Safari FILE` drops a tab into
  # whichever window is frontmost, which would put the fixture among the user's
  # real tabs. Creating an empty window first makes that frontmost window ours.
  SAFARI_WINDOW_ID="$(osa 25 <<'APPLESCRIPT'
tell application "Safari"
  activate
  make new document
  return id of front window
end tell
APPLESCRIPT
)"
  sleep 1
  open -a Safari "$fixture"
  sleep 3

  local url
  url="$(osa1 10 'tell application "Safari" to return URL of current tab of front window')"
  case "$url" in
    *"fixture-$RUN_ID.html") ;;
    *) note "driver could not load the fixture (front tab: ${url:-none})"; return 1 ;;
  esac

  click_front_window_center "Safari"
}

teardown_safari() {
  # Close fixture *tabs*, never a window: closing a window whose current tab is
  # the fixture destroys every other tab in it, and Safari is free to have put
  # our page next to the user's. Closing the last tab closes the window anyway,
  # which is the only case where losing one is correct.
  osa 25 >/dev/null <<APPLESCRIPT || true
tell application "Safari"
  repeat with wi from (count of windows) to 1 by -1
    try
      repeat with ti from (count of tabs of window wi) to 1 by -1
        if (URL of tab ti of window wi) contains "fixture-$RUN_ID.html" then
          close tab ti of window wi
        end if
      end repeat
    end try
  end repeat
end tell
APPLESCRIPT

  # Then the window we created, but only if nothing but blank tabs is left in it.
  if [ -n "$SAFARI_WINDOW_ID" ]; then
    osa 25 >/dev/null <<APPLESCRIPT || true
tell application "Safari"
  repeat with wi from (count of windows) to 1 by -1
    if (id of window wi) is $SAFARI_WINDOW_ID then
      set hasContent to false
      try
        repeat with t in tabs of window wi
          if (URL of t) is not "favorites://" then set hasContent to true
        end repeat
      end try
      if not hasContent then close window wi
    end if
  end repeat
end tell
APPLESCRIPT
    SAFARI_WINDOW_ID=""
  fi
}

# --- driver: Google Chrome ---------------------------------------------------

APP_chrome_bundle="com.google.Chrome"
APP_chrome_process="Google Chrome"
APP_chrome_chars="$CHARS_MIXED"
APP_chrome_match="exact"

setup_chrome() {
  local fixture="$TMP_DIR/fixture-$RUN_ID.html"
  write_html_fixture "$fixture"
  open -a "Google Chrome"
  app_ready "Google Chrome" || return 1

  # Unlike Safari, Chrome does accept AppleScript navigation to file:// URLs, so
  # the fixture can go straight into a window we made and never touches the
  # user's.
  osa 25 >/dev/null <<APPLESCRIPT
tell application "Google Chrome"
  activate
  set w to make new window
  set URL of active tab of w to "file://$fixture"
end tell
APPLESCRIPT
  sleep 3

  local url
  url="$(osa1 10 'tell application "Google Chrome" to return URL of active tab of front window')"
  case "$url" in
    *"fixture-$RUN_ID.html") ;;
    *) note "driver could not load the fixture (front tab: ${url:-none})"; return 1 ;;
  esac

  click_front_window_center "Google Chrome"
}

teardown_chrome() {
  osa 25 >/dev/null <<APPLESCRIPT || true
tell application "Google Chrome"
  repeat with wi from (count of windows) to 1 by -1
    try
      repeat with ti from (count of tabs of window wi) to 1 by -1
        if (URL of tab ti of window wi) contains "fixture-$RUN_ID.html" then
          close tab ti of window wi
        end if
      end repeat
    end try
  end repeat
end tell
APPLESCRIPT
}

# --- driver: Visual Studio Code ----------------------------------------------

APP_vscode_bundle="com.microsoft.VSCode"
APP_vscode_process="Code"
APP_vscode_chars="$CHARS_MIXED"
APP_vscode_match="exact"

setup_vscode() {
  local fixture="$TMP_DIR/fixture-$RUN_ID.txt"
  printf '%s\n' "$FIXTURE_TEXT" > "$fixture"
  open -a "Visual Studio Code" "$fixture"
  # VS Code has no AppleScript object model, so the window title is the only
  # marker available — and a cold start can take far longer than any sleep worth
  # paying on a warm one.
  wait_for_window_title Code "fixture-$RUN_ID" 45 || return 1

  click_front_window_center "Code"
}

teardown_vscode() {
  # No AppleScript object model to match on, so the window title is the only
  # marker available. Cmd-W without this check would close whichever tab the
  # user happens to be on.
  local title
  title="$(front_window_title Code)"
  case "$title" in
    *"fixture-$RUN_ID"*) inject_key "$KEYCODE_W" command ;;
    *) note "left the VS Code tab open: front window is ${title:-none}, not the fixture" ;;
  esac
}

# --- driver: Notes -----------------------------------------------------------

APP_notes_bundle="com.apple.Notes"
APP_notes_process="Notes"
APP_notes_chars="$CHARS_MIXED"
APP_notes_match="exact"

NOTES_CREATED=0

setup_notes() {
  open -a Notes
  app_ready Notes || return 1
  # The run marker is the first line of the body, which Notes also uses as the
  # note's name — so teardown can find this note with a `whose name contains`
  # filter that Notes evaluates internally, instead of pulling the user's whole
  # library into AppleScript to look for it.
  osa 25 >/dev/null <<APPLESCRIPT
tell application "Notes"
  activate
  set newNote to make new note with properties {body:"$FIXTURE_TEXT"}
  show newNote
end tell
APPLESCRIPT
  NOTES_CREATED=1
  sleep 2
  click_front_window_center "Notes"
}

teardown_notes() {
  [ "$NOTES_CREATED" = 1 ] || return 0
  # Delete by index into the match list, not by iterating it: `repeat with n in
  # (notes whose …)` followed by `delete n` silently fails in Notes, which is
  # how the first run left a fixture note behind.
  #
  # Notes' delete is a move to Recently Deleted, so this stays recoverable.
  local result
  result="$(osa 25 <<APPLESCRIPT
tell application "Notes"
  set matches to (notes whose name contains "$RUN_ID")
  repeat while (count of matches) > 0
    delete item 1 of matches
    set matches to (notes whose name contains "$RUN_ID")
  end repeat
  return "ok"
end tell
APPLESCRIPT
)"
  if [ "$result" != "ok" ]; then
    note "could not delete the fixture note; look for '$RUN_ID' in Notes"
  fi
  NOTES_CREATED=0
}

# --- driver: Preview (PDF) ---------------------------------------------------

APP_preview_bundle="com.apple.Preview"
APP_preview_process="Preview"
APP_preview_chars="$CHARS_ASCII"
# texttopdf pads every line to the column width and fills the page with blank
# lines, so Select All returns the fixture plus a lot of whitespace. The length
# check can only be a lower bound; what pins the result to our document is the
# front-document name check in setup.
APP_preview_match="atleast"

setup_preview() {
  local txt="$TMP_DIR/fixture-$RUN_ID.txt" pdf="$TMP_DIR/fixture-$RUN_ID.pdf"
  printf '%s\n' "$FIXTURE_ASCII" > "$txt"
  if ! cupsfilter -i text/plain "$txt" > "$pdf" 2>/dev/null || [ ! -s "$pdf" ]; then
    note "cupsfilter could not produce the PDF fixture"
    return 1
  fi

  open -a Preview "$pdf"
  app_ready Preview 45 || return 1

  # System Events rather than Preview's own dictionary: it answers while Preview
  # is still settling, and it was the only thing still working when Preview
  # stopped pumping AppleEvents during a cold start.
  wait_for_window_title Preview "$RUN_ID" 45 || return 1

  click_front_window_center "Preview"
}

teardown_preview() {
  # Close the fixture's own window by title, through System Events. Preview's
  # `close document` needs the application to be answering events, which is
  # exactly what it was not doing when this driver first failed — and leaving
  # the fixture open next to the user's documents is the worse outcome.
  osa 15 >/dev/null <<APPLESCRIPT
tell application "System Events" to tell process "Preview"
  repeat with w in (windows whose name contains "$RUN_ID")
    try
      click (first button of w whose subrole is "AXCloseButton")
    end try
  end repeat
end tell
APPLESCRIPT
}

# --- driver: Terminal --------------------------------------------------------

APP_terminal_bundle="com.apple.Terminal"
APP_terminal_process="Terminal"
APP_terminal_chars="$CHARS_MIXED"
# Select All takes the window's whole scrollback, which also holds the shell
# prompt before and after the fixture. Lower bound only; the window-id check in
# setup is what proves the selection came from our window and not from the one
# this script is running in.
APP_terminal_match="atleast"

TERMINAL_WINDOW_ID=""

setup_terminal() {
  local fixture="$TMP_DIR/fixture-$RUN_ID.txt"
  printf '%s\n' "$FIXTURE_TEXT" > "$fixture"

  # Terminal may not be running at all, and `do script` against an application
  # that is still launching is what stalled the first run.
  open -a Terminal
  app_ready Terminal 45 || return 1

  TERMINAL_WINDOW_ID="$(osa 25 <<APPLESCRIPT
tell application "Terminal"
  activate
  do script "clear && cat '$fixture'"
  return id of front window
end tell
APPLESCRIPT
)"
  sleep 2

  # This script may itself be running in Terminal. If the new window did not
  # come forward, Select All would grab our own scrollback and the survey would
  # report a reading of the harness rather than of the fixture.
  #
  # Both ids must be present, not merely equal: when the AppleScript above times
  # out and this query fails too, both are empty and a bare `!=` comparison
  # passes — which is how the first run came to read 44 characters of somebody
  # else's shell prompt and report it as a Terminal result.
  local front_id
  front_id="$(osa1 10 'tell application "Terminal" to return id of front window')"
  if [ -z "$TERMINAL_WINDOW_ID" ] || [ -z "$front_id" ]; then
    note "could not identify the fixture window (created: ${TERMINAL_WINDOW_ID:-none}, front: ${front_id:-none})"
    return 1
  fi
  if [ "$front_id" != "$TERMINAL_WINDOW_ID" ]; then
    note "the fixture window is not frontmost (front id: $front_id)"
    return 1
  fi

  click_front_window_center "Terminal"
}

teardown_terminal() {
  [ -n "$TERMINAL_WINDOW_ID" ] || return 0
  # Only our own window, and only while nothing is running in it — closing a
  # busy Terminal window prompts to kill the process.
  osa 25 >/dev/null <<APPLESCRIPT || true
tell application "Terminal"
  repeat with wi from (count of windows) to 1 by -1
    try
      if (id of window wi) is $TERMINAL_WINDOW_ID and not (busy of window wi) then
        close window wi
      end if
    end try
  end repeat
end tell
APPLESCRIPT
  TERMINAL_WINDOW_ID=""
}

# --- the run -----------------------------------------------------------------

run_case() {
  local key="$1"
  local bundle process expected match
  eval "bundle=\$APP_${key}_bundle"
  eval "process=\$APP_${key}_process"
  eval "expected=\$APP_${key}_chars"
  eval "match=\$APP_${key}_match"

  info "$process: staging the fixture"
  if ! "setup_$key"; then
    invalid "$process: driver could not stage the test"
    "teardown_$key" || true
    record_result "$process" "invalid" "" "" "" "" "" "" "" "" "" "driver-setup-failed"
    return
  fi

  select_all
  sleep 0.5

  # Plan §4.2: a case where the target lost the foreground proves nothing, and
  # must not be counted as a product failure either.
  local front
  front="$(frontmost_bundle_id)"
  if [ "$front" != "$bundle" ]; then
    invalid "$process: not frontmost at injection time (got ${front:-none})"
    "teardown_$key" || true
    record_result "$process" "invalid" "" "" "" "" "" "" "" "" "" "lost-foreground"
    return
  fi

  local offset
  offset="$(log_size)"
  info "$process: injecting the read hotkey"
  trigger_read_hotkey

  if ! wait_for_event "$offset" "event=selection_(ok|err)" "$SPEAK_TIMEOUT_S"; then
    fail "$process: VoiceX logged no selection result"
    log_since "$offset" | grep -E 'event=' | sed 's/^/     /' || true
    "teardown_$key" || true
    record_result "$process" "fail" "" "" "" "" "" "" "" "" "" "no-log-response"
    return
  fi

  local window ax_line ok_line role attr status range marker
  window="$(log_since "$offset")"
  ax_line="$(echo "$window" | grep -E 'event=selection_ax' | tail -1 || true)"
  role="$(echo "$ax_line"   | field role)"
  attr="$(echo "$ax_line"   | field attr)"
  status="$(echo "$ax_line" | field status)"
  range="$(echo "$ax_line"  | field has_sel_range)"
  marker="$(echo "$ax_line" | field has_marker_range)"
  # The attribute probe is skipped when the AX read already produced text, so an
  # empty column there means "not asked", not "not supported".
  [ -n "$range" ]  || range="n/a"
  [ -n "$marker" ] || marker="n/a"

  ok_line="$(echo "$window" | grep -E 'event=selection_ok' | tail -1 || true)"
  if [ -z "$ok_line" ]; then
    local err
    err="$(echo "$window" | grep -E 'event=selection_err' | tail -1 | field error)"
    fail "$process: selection failed (${err:-unknown})"
    record_result "$process" "fail" "none" "" "$role" "$attr" "$status" "$range" "$marker" "" "" "${err:-unknown}"
    "teardown_$key" || true
    return
  fi

  local source sens chars elapsed restored
  source="$(echo   "$ok_line" | field source)"
  sens="$(echo     "$ok_line" | field sensitivity)"
  chars="$(echo    "$ok_line" | field chars)"
  elapsed="$(echo  "$ok_line" | field elapsed_ms)"
  restored="$(echo "$ok_line" | field clipboard_restored)"
  case "$restored" in
    true)  restored="restored" ;;
    false) restored="LOST" ;;
    *)     restored="untouched" ;;
  esac

  local result detail=""
  if [ "$match" = exact ] && [ "$chars" = "$expected" ]; then
    result=pass
    pass "$process: read the fixture exactly ($chars chars) via $source/$sens"
  elif [ "$match" = atleast ] && [ "$chars" -ge "$expected" ]; then
    result=pass
    detail="length>=$expected only"
    pass "$process: read $chars chars (>= $expected) via $source/$sens"
    note "length is a lower bound here; Select All legitimately picks up more than the fixture"
  else
    result=fail
    detail="expected $match $expected, got $chars"
    fail "$process: read the wrong text ($detail)"
  fi
  note "$ok_line"

  if wait_for_event "$offset" 'event=speak_started' "$SPEAK_TIMEOUT_S"; then
    local stop_offset
    stop_offset="$(log_size)"
    trigger_read_hotkey
    if ! wait_for_event "$stop_offset" 'event=speak_stopped reason=hotkey' "$STOP_TIMEOUT_S"; then
      fail "$process: the second press did not stop speech"
      result=fail
      detail="${detail:+$detail; }stop-failed"
    fi
  else
    fail "$process: speech never started"
    result=fail
    detail="${detail:+$detail; }speak-failed"
  fi

  record_result "$process" "$result" "$source" "$sens" "$role" "$attr" "$status" \
                "$range" "$marker" "$elapsed" "$restored" "${detail:--}"

  "teardown_$key" || true
  sleep 0.5
}

# --- preflight ---------------------------------------------------------------

if [ ! -f "$LOG_FILE" ]; then
  echo "Log file not found: $LOG_FILE" >&2
  echo "Start VoiceX with:  pnpm tauri dev 2>&1 | tee $LOG_FILE" >&2
  exit 2
fi

if ! grep -q 'Selected-text reading hotkey' "$LOG_FILE"; then
  echo "The log shows no VoiceX startup with selected-text reading enabled." >&2
  echo "Restart VoiceX and make sure its stderr goes to $LOG_FILE." >&2
  exit 2
fi

for key in $TARGET_APPS; do
  if ! declare -F "setup_$key" >/dev/null; then
    echo "Unknown application: $key (see --list)" >&2
    exit 2
  fi
done

info "Log file: $LOG_FILE"
info "Run marker: $RUN_ID"
results_init "$TMP_DIR/results.tsv"

for key in $TARGET_APPS; do
  run_case "$key"
done

print_summary
echo
note "path/sens/role/attr are the survey's actual output; result only says whether the chain ran."
note "atleast rows (Preview, Terminal) assert a lower bound on length, not the exact text."

if [ "$INVALID" -gt 0 ]; then
  invalid "$INVALID case(s) never ran — the survey is incomplete"
fi
if [ "$FAILURES" -gt 0 ]; then
  fail "$FAILURES check(s) failed"
fi
[ "$FAILURES" -eq 0 ] && [ "$INVALID" -eq 0 ]
