# Verification — round-3 P1 fix (clipboard subprocess deadline)

Date: 2026-09-17. Commits: `13297f3` (fix + regression tests), `d32cf3b`
(review follow-ups). Scope: `git diff b44eb37...HEAD`.

## What changed

`backend::clipboard_set` now runs the complete stdin-write + child-wait as
one operation (`tokio::join!`) raced in a single `tokio::select!` against a
5 s deadline (`WL_COPY_TIMEOUT`) and the abort signal. On timeout or
cancellation the `op` future is dropped (closing stdin — `ChildStdin`
delivers EOF only on drop, not on `shutdown()`), then `child.kill().await`
SIGKILLs and reaps wl-copy before the function returns and before the
action lock can pass to another action. Post-spawn failures still map to
`Delivery::Unknown` (`effect: "unknown"`), since the clipboard may already
have been replaced; only pre-spawn `Cancelled`/`Missing` report `none`.

`type_text` payloads are now bounded at 1 MiB (`PAYLOAD_TOO_LARGE`,
`effect: none`, before wl-copy spawns), satisfying the spec's payload-size
bound.

`wedged_input_connection_times_out_and_recovers` now synchronizes on the
silent socket's accept channel and requires `CANCELLED` exactly — an
unrelated backend setup failure can no longer satisfy it. Request ids in
the queued-cancellation regression renamed to `wedged`/`queued`.

## Commands and results

- `cargo test`: **13 unit + 13 stdio integration tests pass** (live
  Hyprland session). New tests:
  - `clipboard_timeout_kills_delayed_side_effect_child`: wl-copy reads the
    text, sleeps 7 s, then writes a marker. Server responds ~5.6 s with
    `effect: unknown`, `do_not_replay`; marker never appears (child killed
    and reaped).
  - `clipboard_blocked_stdin_is_bounded_and_cancellable`: wl-copy never
    reads stdin; 256 KiB payload blocks the pipe. Request returns at the
    deadline with `effect: unknown`; a follow-up action answers
    `STALE_OBSERVATION` promptly. Phase 2 cancels mid-blocked-write; the
    follow-up responds <4 s and the child's pid is absent from `/proc`
    (killed and reaped). Also asserts >1 MiB text is `PAYLOAD_TOO_LARGE`
    with `effect: none` and no wl-copy spawn.
- `cargo fmt --check`, `git diff --check`: clean.
- `python3 .scratch/computer-use-mcp/review-evidence/round3-clipboard.py`
  (the reviewer's own reproduction harness, unchanged):
  - `late-side-effect`: response 5.62 s, `effect: unknown`,
    `marker_at_response: false`, `marker_after_timeout: false` (was `true`
    in round 3).
  - `blocked-stdin`: response at 5.63 s (was: no response in 6.5 s), and
    the following action answers `STALE_OBSERVATION` after cancellation
    (was: no response).
  - Output saved to `review-evidence/round4-clipboard-results.jsonl`.

## Two-axis review (b44eb37...HEAD)

- Spec axis: all P1 requirements met; one partial — the cancellation-half
  kill was initially unverified (fixed in d32cf3b via the pid/`/proc`
  check); payload bound was missing (fixed in d32cf3b).
- Standards axis: no hard violations. Applied: `WL_COPY_TIMEOUT` naming,
  `silent_listener()` dedup, `first_monitor()` in the wedged test. Noted
  judgement calls kept: `Stop::Cancelled` maps to `Failed` (deliberate —
  post-spawn cancel is `unknown`, not `none`); `wait_cancelled()` resolves
  on cancel or wake, a silent generation move degrades to the deadline.

## Limits

- All clipboard evidence uses controlled wl-copy substitutes on PATH
  (marker files, never the real clipboard) plus the reviewer's unchanged
  harness. No real clipboard delivery was exercised in this round.
- Actual Codex MCP/GUI evidence remains as previously reported on #6; this
  change did not repeat the Codex roundtrip or physical multi-monitor
  checks.
- `key_and_text_rejections` flaked once in a full-suite parallel run with
  `EVENT_CHANNEL_UNHEALTHY` (event watcher had not connected yet when the
  first action ran; passes consistently alone, 3/3). Pre-existing
  synchronization gap in that test's setup, not introduced by this diff.
- wl-copy's real daemonized selection source, once forked, is outside
  SIGKILL reach of the spawned process — `effect: unknown` honestly covers
  the case where the clipboard may already have been served.
