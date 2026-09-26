# Jev Fast Path

Status: implemented as an opt-in bounded Fast Path. Live validation covers
native GTK/XWayland, Chrome/Firefox, and English/Korean OCR at confidence 0.90.
The supported envelope and controlled same-consumer measurements are recorded
in [live validation](jev-fast-path-live-validation.md). This is not unrestricted
application automation; unverified sources or effects return to the original tools.

`computer_goal` is an opt-in, bounded addition to the existing MCP. It is
not required by `computer_monitors`, `computer_observe`, or `computer_action`.
Without `TYPESAFE_API_KEY`, an enabled evidence source, or a supported target,
the tool hands control back without sending input.

## Request schema

The exact additive MCP tool input is:

```json
{
  "observation_id": "opaque-id-from-computer_observe",
  "target": {
    "application": "Fixture App",
    "window": "Native Fixture",
    "window_id": "optional-stable-atspi-window-id"
  },
  "authorization": "navigation",
  "goal": "select the benign Show completion tab",
  "permitted_interactions": ["click"],
  "approved_literals": [],
  "completion": {
    "name": "Done",
    "role": "status",
    "state": "visible"
  },
  "limits": {
    "max_actions": 1,
    "timeout_ms": 30000
  }
}
```

`target.application` and `target.window` bind the request to the selected
evidence source. Native accessibility uses its application and window names;
browser evidence uses the current browser and active-tab title; local OCR uses
the exact focused window identity reported by Hyprland. OCR requires
`target.window`; `window_id`, when present, must match the current source.

`authorization` is required: `navigation`, `non_sensitive_editing`, or
`media_playback`. `navigation` permits only grounded `tab` and
`page tab` selection from native accessibility or an explicitly trusted browser
origin. Browser actions require `COMPUTER_USE_BROWSER_ALLOWED_ORIGINS`, a JSON
array of exact canonical HTTP(S) origins configured in the MCP server environment.
No origin is enabled by default. A page, extension message, or MCP Goal cannot
supply this authorization. Otherwise the Goal returns `no_permitted_candidates`
without calling Jev or sending input (unless completion is already observed).
Generic links and menu items do not establish their
consequences, so the Fast Path hands back without creating a candidate.
`non_sensitive_editing` permits caller-approved text entry into an already
focused editable control. Generic check boxes, radio buttons, and toggle
buttons do not expose their consequence, so they remain unavailable until the
server can verify the exact authorized effect.
`permitted_interactions` is a closed enum: `click` and `type_text`. The native
path reobserves after each physical Action and executes at most 10 Actions per
Goal. `max_actions` can lower that limit. `timeout_ms` is shared across
extraction, inference, reobservation, verification, and dispatch, and is capped
at 30 seconds. Refresh-only loops are capped at 10 reobservations.
`approved_literals` has the shape `{ "id": "search-term", "text": "..." }`.
Literal values are local-only, must be caller-approved non-secret UTF-8, and
are never projected to Jev or logs; all approved values together are capped at
1 MiB. Literal IDs are local labels and are not projected either. `type_text`
candidates are generated only
for a visible, enabled, established, focused, non-protected editable control.
AT-SPI role names alone never establish editability; the helper requires the
`EDITABLE` state, and it suppresses the accessible names of text widgets.

`completion` is checked against fresh evidence from the selected source after
each Action; it is not inferred from Jev's choice. Its `state` is one of `visible`,
`enabled`, `focused`, `selected`, `checked`, or `expanded`.
`limits.timeout_ms` covers evidence extraction, inference, revalidation, and
dispatch and is capped at 30 seconds.

## Bounded model projection

The server sends TypeSafe a `Choice` with at most 64 server-created candidate
IDs plus the reserved `reobserve` and `abstain` options. The state contains
only the bounded Goal, target application, observation/evidence
revisions, candidate IDs/descriptions, and descriptions of actions already
delivered during this Goal. This history prevents restarting a multi-step Goal;
it contains no literal values and does not itself prove completion. It never contains screenshots,
coordinates, generated keys or commands, existing editable values, approved
literal values, password/protected fields, clipboard data, document bodies,
unrelated windows, or environment data. Labels are untrusted evidence and
cannot change the permission envelope. Keyword filters are deny-only; the
explicit authorization scope and native role/capability checks establish what
can be a candidate.

The documented TypeSafe endpoint is `POST
https://api.typesafe.ai/v1/systemone` with `model: "jev-latest"` and a Choice
question named `action`. For credential-free tests,
`COMPUTER_USE_TYPESAFE_URL` points the server at a controlled HTTP endpoint.
The API key is read only from the server's `TYPESAFE_API_KEY` environment
variable. The default confidence floor is `0.90`; it can be lowered only by
the server operator with `COMPUTER_USE_JEV_CONFIDENCE_THRESHOLD`. A selected
candidate must also have probability at or above that floor and be the unique
highest-probability option. A tie, a lower selected probability, or low
confidence returns a handback without input.

## Evidence sources

`source` defaults to `native_accessibility`. Select `browser_extension` or
`local_ocr` only when that source is explicitly enabled for the current task.
All three sources use the same bounded action flow; browser input additionally
requires the server-owned origin policy.
All sources retain target validation, deadlines, completion checks, and handback. Source identity and evidence revisions are
bound to each selected candidate.

### Current browser tab

The extension asks only for `activeTab`, `scripting`, and `nativeMessaging`.
Click its toolbar action to enable evidence for that tab; click it again to
stop. Tab changes, window focus loss, and page navigation invalidate the saved
projection. The extension forwards visible bounded tabs, focused editable
controls, and status labels. It sends the current origin to the local server
from the browser tab API, not from page-supplied DOM data. URLs, origins, page
bodies, form values, cookies, and screenshots are never included in Jev requests.
The local server receives only the origin (no path/query) for policy matching.

Configure only an application you trust to keep these tab/input handlers within
the requested low-consequence scope. An origin is a trust boundary, not proof of
what arbitrary JavaScript does. Do not allow a general browsing origin whose
handlers can submit, purchase, delete, or otherwise perform excluded effects.

```sh
export COMPUTER_USE_BROWSER_ALLOWED_ORIGINS='["https://trusted-app.example"]'
```

The match includes scheme, hostname, and non-default port. Wildcards, URL paths,
and prefix matching are not accepted. Revocation takes effect at revalidation.
The extension's toolbar action is still required separately for each tab session.

On Linux, build and package the extension, then register its native messaging
host:

```sh
cargo build --release
scripts/package-jev-extension firefox
scripts/install-jev-native-host firefox
```

In Firefox, open `about:debugging#/runtime/this-firefox`, choose **Load
Temporary Add-on**, and select `target/jev-fast-path/firefox/manifest.json`.
For Google Chrome, run the same two scripts with `chrome`, then load
`target/jev-fast-path/chrome` from `chrome://extensions` with Developer mode
enabled. These scripts prepare local files only when run; they do not install
or enable an extension automatically. Remove the extension and its user-level
native host manifest to disable the integration.

The native host accepts only the Firefox ID or the fixed Chrome extension ID
in the matching host manifest. It checks the parent process against the
corresponding browser executable and requires that executable and every parent
directory to be root-owned and not group- or world-writable. A browser-named
executable in a user-writable location and a matching argument from a directly
invoked local process are rejected. The host sends its bounded projection over
a private runtime socket. The MCP verifies the connecting process executable,
browser parent, extension ID, and user before retaining evidence in memory;
same-user processes cannot inject evidence by replacing a runtime file.
Evidence expires after three seconds. Firefox viewport coordinates use the
verified inner-screen origin on XWayland. On native Wayland, validated Firefox
reports a window-relative inner-screen origin; the MCP anchors that offset to
the unique focused Hyprland client and checks that the viewport fits inside
the client bounds. Chrome uses the AT-SPI web-document rectangle from the exact
focused compositor PID and page title. XWayland physical frame/document bounds
and native-Wayland frame/document coordinate contracts are checked separately
against Hyprland and CSS viewport dimensions; offsets are never guessed from
`screenX/screenY`. Unknown mappings, zoom/scale mismatches, transformed displays,
or ambiguous same-title windows hand back.

Chrome needs Python GI/AT-SPI and browser accessibility enabled (for example,
start the browser with `--force-renderer-accessibility` and without
`NO_AT_BRIDGE=1`). A custom Chrome `--user-data-dir` uses its own
`NativeMessagingHosts/ai.typesafe.computer_use.json`; the install script targets
the normal default profile location. Validation used temporary profiles only.

### Local OCR

Local OCR requires the caller to name one current-Observation crop and affirm
that it contains non-sensitive navigation labels. For example:

```json
{
  "source": "local_ocr",
  "ocr_region": {
    "x": 40,
    "y": 80,
    "width": 640,
    "height": 180,
    "non_sensitive": true,
    "purpose": "navigation_tabs"
  },
  "ocr_languages": ["eng", "kor"]
}
```

The rectangle is in screenshot pixels on the same monitor Observation. The
caller must identify it as a non-sensitive tab-control region and authorize
clicks on those tabs. The region must fit inside the focused target window on
an unrotated, verified display. The code cannot determine whether a crop
contains a secret; the caller-provided `non_sensitive` value is an explicit
scope declaration, not a secret detector. OCR runs locally with Python,
Pillow, Tesseract, and the requested `eng` and/or `kor` trained data. No OCR
system packages are installed automatically. Missing programs or language data
return a handback while the original three tools continue to work.

For crops up to 1 megapixel with a longest edge no greater than 4096 pixels,
the helper enlarges the Tesseract input 2× and maps recognized bounds back to
the original crop. Tesseract TSV output is read with a 1 MiB retained-output
limit; overflow terminates extraction and returns a handback. Revalidation
binds candidates to a normalized RGB
fingerprint of the original crop. Downsampling and 16-level channel
quantization reduce sensitivity to some capture noise. A small pixel change
can still cross a quantization boundary and change the fingerprint, while a
subtle change below the 128-pixel normalized resolution or within one
quantization bucket may go undetected.

The initial extraction crops the exact screenshot bytes stored with the
`computer_observe` Observation the caller supplied. Screenshots over 20 MiB
are not retained for OCR and return a handback. The full screenshot and crop
stay in process memory and the crop is piped only to local Tesseract. TypeSafe
receives high-confidence OCR labels and candidate descriptions, never
screenshot bytes or OCR image data. OCR text remains untrusted: this source can
produce only caller-authorized navigation-tab clicks; it cannot establish
typing, submission, or other effects. OCR text cannot prove completion. After a
click, completion must be found in fresh native accessibility evidence;
otherwise the Goal returns a handback with its action history. Revalidation
takes a fresh screenshot and reruns OCR over the same crop before dispatch. The
current evidence uses a minimum OCR confidence of 70/100; ambiguous or
unsupported results hand back.

## Result shape

A successful Goal returns a result like:

```json
{
  "outcome": "completed",
  "status": "completed",
  "reason": "completion_verified_from_fresh_native_evidence",
  "effect": "completed",
  "do_not_replay": false,
  "actions_executed": 1,
  "action_history": [{
    "candidate_id": "candidate-...",
    "target_element_id": "window/0",
    "action": {"kind": "click", "literal": "not_applicable"},
    "effect": "completed",
    "do_not_replay": false
  }],
  "progress": {
    "goal_complete": true,
    "completion_observed": true,
    "max_actions": 1,
    "selected_candidate_id": "candidate-..."
  },
  "selection": {
    "candidate_id": "candidate-...",
    "confidence": 0.98,
    "probability": 0.99,
    "model": "jev-1.13.0",
    "usage": {"input_tokens": 200, "output_tokens": 30}
  },
  "timings_ms": {
    "extraction_ms": 12,
    "inference_ms": 180,
    "revalidation_ms": 14,
    "input_ms": 220,
    "capture_ms": 30,
    "total_ms": 430
  },
  "observation": {"observation_id": "fresh-id"}
}
```

`action_history` and `steps` retain every bounded attempt, including earlier
completed Actions if a later step hands back. Type-text literals are represented
only as `redacted`. A handback after any delivered Action sets the Goal-level
`do_not_replay: true`; assess the returned history and current UI before resuming.
MCP cancellation drops the in-flight Goal result. If the caller cancels or
disconnects after dispatch, earlier Actions may already have taken effect and
their history may not be returned. Call `computer_observe`, inspect the current
UI and independent task state, and verify progress before resuming; never replay
the cancelled Goal or an Action just because its response was absent.
Repeated non-progress, unsupported completion evidence, changed focus/source,
low confidence, and the shared deadline stop the loop. Missing credentials, unsupported or stale
accessibility evidence, blocked/consequential Goals, no candidates,
uncertainty, malformed/API failures, changed focus/source, and reserved model
choices all hand back with `effect: "none"` before input. Partial or unknown existing input delivery is
reported as such and is never safe to replay blindly.

An input-dispatch attempt invalidates its source Observation before releasing
the shared action lock. This also invalidates a Jev choice that was still
pending against that Observation. If the backend fails before confirming
delivery (`effect: "none"`), take a fresh Observation before retrying; the old
one cannot authorize another attempt. Pre-dispatch validation rejections keep
the Observation valid. This invalidation does not isolate the desktop from
other input actors.

## Local evidence fixture

Credential-free integration tests can set
`COMPUTER_USE_ATSPI_EVIDENCE=/absolute/path/evidence.json`. The fixture uses
this bounded shape:

```json
{
  "source": {
    "application": "Fixture App",
    "window": "Native Fixture",
    "window_id": "fixture-window-1",
    "revision": "rev-1",
    "visible": true,
    "focused": true,
    "occluded": false
  },
  "coordinate_space": "desktop_logical",
  "truncated": false,
  "elements": [
    {
      "id": "window/0",
      "role": "page tab",
      "name": "Show completion",
      "x": 100,
      "y": 100,
      "width": 120,
      "height": 40,
      "visible": true,
      "enabled": true,
      "showing": true,
      "focused": false,
      "editable": false,
      "protected": false
    }
  ]
}
```

This test seam supplies evidence only; monitor snapshots, observation
freshness, focus/display health, input serialization, and the existing
post-action screenshot path remain real server behavior. The fixture must set `truncated: false` and must not be enabled in a
user's MCP environment.

## Native coordinate support

The AT-SPI helper is read-only and bounded. It requires a showing, active target;
this is its explicit occlusion assumption, not a universal overlay detector.
For XWayland the server verifies the native process/window identity and all four
AT-SPI frame extents against Hyprland physical bounds before converting to desktop
logical coordinates. This path needs no evidence-file adapter. A different
process, moved/mismatched frame, or rotated display is rejected. Native Wayland
toolkit SCREEN contracts vary, so an unknown native mapping remains a handback.
An operator may set `COMPUTER_USE_ATSPI_COORDINATE_SPACE=desktop_logical` only for
a separately verified toolkit mapping. OCR completion does not infer coordinates
from AT-SPI, but still binds completion to the original process/window.

OCR revalidation compares every RGB channel in the approved crop against the
original Observation. A per-channel difference no larger than one is treated as
capture dithering and uses the original crop for deterministic OCR and hashing.
There is no accumulated tolerance across refreshes. Larger differences use the
fresh crop and retain normal fingerprint, text, bounds, confidence, focus, and
window checks. The existing coarse fingerprint is not a pixel-perfect change
detector; all this processing is local, in memory, and never sent to TypeSafe.

### Firefox Netflix playback

`media_playback` permits only adapter-recognized Netflix title and Play controls
from `browser_extension` evidence. Set
`COMPUTER_USE_BROWSER_ALLOWED_ORIGINS='["https://www.netflix.com"]'` on the server
and enable the extension for the active tab. Account controls and arbitrary
buttons are excluded. Use completion
`{"name":"Media playback active","role":"status","state":"visible"}`.
The adapter checks a visible episode video on `/watch/<id>` with advancing time,
sufficient ready state, and neither paused nor ended; previews do not count.
After its initial post-click observation, the server retries completion checks
for up to three additional seconds within the Goal deadline, observing without
repeating input.

The Firefox build loaded through `about:debugging` is temporary: it is removed
on Firefox exit. Native-host registration persists, but normal permanent
installation requires a signed extension. Signing checks are not disabled.
