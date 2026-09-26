# Jev Fast Path live validation — 2026-09-26

Direct implementation and verification completed in the current Codex task.
No Devin was used. This report supersedes the earlier read-only-browser
finalization and low-threshold OCR reports; historical raw evidence is retained.

## Implemented and exercised

- Native GTK/XWayland: exact PID/title/frame-to-compositor mapping in the production
  helper, with no evidence-file adapter. Two-step Goal completed 3/3 final samples.
- Chrome: real extension, authenticated native host, real Jev and physical input;
  Wayland and XWayland viewport geometry verified. One/two-step navigation and
  caller-supplied text entry completed with independent fixture state checks.
- Firefox: the same production extension scripts completed two-step navigation
  and text entry in a temporary Wayland profile.
- English/Korean OCR: local Tesseract 5.5.3, visual canvas targets absent from AT-SPI
  controls, default confidence 0.90, real Jev, physical clicks and fresh native
  completion. Final production-helper samples completed 3/3 per language.
- Browser input now requires an exact origin in the server operator's allowlist.
  The extension's active-tab permission is necessary but not sufficient. Default
  denial and spoofed client authorization are covered. This delegates trust in
  the selected application's handlers; it does not prove arbitrary JavaScript benign.
- Multi-step Jev selection receives already-delivered candidate descriptions,
  with literal values redacted. Fresh evidence still determines completion.

## Matched consuming-model comparison

The consuming model is this GPT-6 Astra task in both arms. Baseline coordinates
were chosen by actually viewing fixture-only screenshots, then calling the original
`computer_action`. Fast Path used one `computer_goal` after `computer_observe`.
All 24 final samples completed: 12 per arm, with zero fallback and zero wrong-target
input in that final sample set. Native/browser tasks have two actions; OCR has one.

| Case | Jev median / p95 (s) | Screenshot median / p95 (s) | Success each arm |
| --- | ---: | ---: | ---: |
| native | 1.964 / 2.034 | 16.517 / 20.559 | 3/3 |
| browser | 2.941 / 3.051 | 21.399 / 34.436 | 3/3 |
| ocr-english | 2.103 / 2.172 | 11.290 / 16.379 | 3/3 |
| ocr-korean | 2.118 / 2.213 | 11.016 / 11.246 | 3/3 |

Time starts at the initial Observe and ends at the final tool response/independent
completion read. Startup and profile setup are excluded. Baseline includes actual
model/tool orchestration and scheduling, so these are workflow measurements, not
an isolated model-latency comparison. OCR fast timings include an additional
preflight diagnostic extraction. Native fast samples overlapped deterministic
regression-test CPU load. There are only three samples per cell: nearest-rank p95
is the maximum, not a reliable estimate of population tail latency.

For two-step tasks, consuming-model action decisions drop from two to one Goal
submission; MCP calls drop from three to two. For one-step OCR, both use two MCP
calls and one consuming-model decision. Jev itself still performs one inference
per attempted step. Do not describe the OCR result as fewer model rounds.

The comparison uses disposable windows/profiles on the same shared Hyprland desktop
(scale 1.25), not the proposed isolated VM. It is a controlled fixture comparison,
not evidence of speed or accuracy across arbitrary applications. The VM-isolation
and representative real-application release criteria are not claimed as passed.
The implementation, live checks, and matched measurements are complete for the
documented supported envelope; these limits must accompany any broader release claim.

## Negative and failure evidence

Real Chrome tests held a live Jev response while switching to a same-title tab,
navigating to a new same-title document, or moving the target. Each returned
`candidate_stale_or_missing`, executed zero actions, and left fixture state unchanged.
An origin omitted from the allowlist made zero provider requests and zero actions.
A native fixture withheld the expected completion after both steps; the Goal
returned handback with its executed history and no false success.

Six held-out live selector cases covered English/Korean matches and missing targets,
duplicate labels, and prompt-injection text. Positive cases selected the right
candidate; negative cases produced no accepted action. The duplicate low-confidence
abstention is a safe handback, not evidence of model certainty.

Nine additional real Firefox/native-host/MCP boundary cases passed, using a
controlled extension and compositor shims. They cover malformed geometry and
provenance, stale/wrong request identifiers, inactive tabs, default-denied tab and
text input, and already-visible completion. No input connections or provider
requests occurred. These are protocol-boundary tests, separate from the visible
browser race tests above.

Earlier development failures are retained rather than counted as final successes:
missing temporary browser host permissions, missing multi-step context, intermittent
instantaneous GTK clicks, and OCR capture dithering. The first final-measurement
attempt before the dithering fix had 2/6 OCR successes and four zero-input stale
handbacks. Pixel comparison found only +/-1 RGB differences. The fix compares every
pixel against the original approved crop, tolerates only that one-level difference,
and does not accumulate tolerance. The subsequent six planned samples all passed.

## Automated checks and artifacts

- Full serial Rust suite: 66 unit tests and 45 stdio runner entries passed; one
  conditional native-live test body returns early unless explicitly enabled.
  The independent visible native tests above exercise the actual production helper.
- After the final OCR change, all 11 OCR stdio regressions passed again, including
  changed crop, changed confidence, focus, geometry, process replacement, timeout,
  missing dependency, and wrong-process completion. Unit tests passed again.
- Four Node extension tests, JavaScript syntax, Python OCR extraction/fingerprint/
  bounded-output self-tests, formatting, type checking, and whitespace checks passed.
- Optimized server and Chrome/Firefox extension bundles were built locally.

Reproducible drivers and raw results are under
[completion evidence](../.scratch/jev-fast-path/completion/).
The exact benchmark inputs/results are in
[benchmark.json](../.scratch/jev-fast-path/completion/benchmark.json).
Final build identity and protocol smoke are recorded in
[final-report.md](../.scratch/jev-fast-path/final-report.md).
No personal browser extension/profile was installed, no commit/PR was created,
and GitHub issue states were not changed.
