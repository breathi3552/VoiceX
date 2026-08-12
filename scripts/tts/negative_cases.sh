#!/usr/bin/env bash
#
# Negative-path cases for selected-text reading (plan §7 step 6).
#
# The positive path is covered by p0_survey.sh across seven applications. What
# was never covered is what happens when there is nothing to read — and those
# are the paths a user hits by accident, so a wrong answer there is worse than a
# missing feature. Each case asserts the specific error code, not merely that
# something failed: "no selection" and "we could not reach the control" send the
# user to entirely different places.
#
# Usage:
#   pnpm tauri dev 2>&1 | tee /tmp/voicex-tts.log      # from your own terminal
#   scripts/tts/negative_cases.sh --log /tmp/voicex-tts.log
#
# Must be run from a terminal you have granted Accessibility and Input
# Monitoring: an unbundled dev build inherits the launching process's grant
# (plan §4.4 #1), so launching it from elsewhere makes every case fail for a
# reason that has nothing to do with the product.

set -uo pipefail

_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$_DIR/lib.sh"

LOG_FILE="${VOICEX_LOG:-/tmp/voicex-tts.log}"
EVENT_TIMEOUT_S=8

while [ $# -gt 0 ]; do
  case "$1" in
    --log) LOG_FILE="$2"; shift 2 ;;
    -h|--help) sed -n '3,20p' "$0"; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

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

# --- case runner -------------------------------------------------------------

# Fire the reading hotkey and assert which error code comes back.
#
# Asserting the code and not just "an error happened" is the point: every one of
# these has a different remedy, and the HUD shows a different message for each.
expect_error() {
  local name="$1" expected="$2" offset line actual

  offset="$(log_size)"
  trigger_read_hotkey

  if ! wait_for_event "$offset" 'event=selection_err' "$EVENT_TIMEOUT_S"; then
    # No error and no speech either: the hotkey never reached the app, which
    # says nothing about the product (plan §4.3).
    if log_since "$offset" | grep -q 'event=hotkey_action'; then
      fail "$name: the read started but reported no error"
    else
      invalid "$name: the hotkey never arrived — check Accessibility permission"
    fi
    return
  fi

  line="$(log_since "$offset" | grep -E 'event=selection_err' | tail -1)"
  actual="$(echo "$line" | field error)"

  if [ "$actual" = "$expected" ]; then
    pass "$name: $actual"
  else
    fail "$name: expected $expected, got ${actual:-none}"
  fi

  # Nothing may be spoken on a failed read.
  if log_since "$offset" | grep -q 'event=speak_started'; then
    fail "$name: spoke despite failing to read a selection"
  fi
}

# --- cases -------------------------------------------------------------------

info "Log file: $LOG_FILE"
info "Run marker: $RUN_ID"

# 1. Focus on VoiceX itself.
#
# Reading our own window would be a loop with no purpose, and the selection
# reader is supposed to notice before doing any work. This case needs no
# fixture, which is also why it runs first: if the hotkey does not arrive here,
# nothing below is worth running either.
info "case: focus on VoiceX itself"
if osa 10 <<'APPLESCRIPT' >/dev/null
tell application "System Events"
  set voicex to first process whose name is "voicex" or name is "VoiceX"
  set frontmost of voicex to true
end tell
APPLESCRIPT
then
  sleep 0.6
  expect_error "focus-is-self" "focus_is_self"
else
  invalid "focus-is-self: could not bring VoiceX forward"
fi

# 2. A text editor with the caret parked and nothing selected.
#
# The most common accident: pressing the hotkey before selecting anything. The
# Accessibility layer answers authoritatively here, so this must come back as
# "nothing is selected" rather than as an unsupported control — the difference
# between "select some text first" and "this app cannot be read".
info "case: empty selection in TextEdit"
FIXTURE="$TMP_DIR/empty-$RUN_ID.txt"
printf 'voicex negative case %s\n' "$RUN_ID" > "$FIXTURE"

if osa 20 <<APPLESCRIPT >/dev/null
tell application "TextEdit"
  activate
  open POSIX file "$FIXTURE"
end tell
APPLESCRIPT
then
  if app_ready "TextEdit" 10 && wait_for_window_title "TextEdit" "$RUN_ID" 10; then
    click_front_window_center "TextEdit" || true
    # Collapse any selection the click may have made, without selecting anything.
    inject_key "$KEYCODE_A" command
    inject_key 123   # Left arrow: caret to the start, selection cleared.
    sleep 0.4
    expect_error "empty-selection" "no_selection"
  else
    invalid "empty-selection: TextEdit never showed the fixture window"
  fi

  osa 10 <<APPLESCRIPT >/dev/null
tell application "TextEdit"
  repeat while (exists (first document whose name contains "$RUN_ID"))
    close (first document whose name contains "$RUN_ID") saving no
  end repeat
end tell
APPLESCRIPT
else
  invalid "empty-selection: could not open the TextEdit fixture"
fi

# --- summary -----------------------------------------------------------------

echo
note "Not covered here: clipboard types that cannot be snapshotted (promised"
note "files, lazily-provided data). Staging one needs an application that"
note "actually offers a file promise, so it stays a manual check for now; the"
note "refusal rules themselves are unit-tested in selection/macos/clipboard.rs."

if [ "$INVALID" -gt 0 ]; then
  invalid "$INVALID case(s) never ran — the negative pass is incomplete"
fi
if [ "$FAILURES" -gt 0 ]; then
  exit 1
fi
[ "$INVALID" -gt 0 ] && exit 2
exit 0
