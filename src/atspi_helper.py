import hashlib
import json
import os
import sys

try:
    import gi
    gi.require_version("Atspi", "2.0")
    from gi.repository import Atspi
except Exception:
    print(json.dumps({"error": "atspi_unavailable"}))
    raise SystemExit(0)

wanted_application = sys.argv[1]
wanted_window = sys.argv[2]
mode = sys.argv[3] if len(sys.argv) > 3 else "actions"
wanted_completion = sys.argv[4] if mode == "completion" and len(sys.argv) > 4 else ""
wanted_completion_role = sys.argv[5] if mode == "completion" and len(sys.argv) > 5 else ""
completion_only = mode == "completion"

try:
    Atspi.init()
except Exception:
    print(json.dumps({"error": "atspi_init_failed"}))
    raise SystemExit(0)

WINDOW_ROLES = {
    "frame", "dialog", "window", "desktop frame", "internal frame", "alert"
}
CONTROL_ROLES = {
    "button", "push button", "link", "check box", "radio button",
    "toggle button", "menu item", "list item", "tab", "page tab",
    "combo box", "entry", "text", "text field", "status bar", "status",
    "label", "notification"
}
PASSWORD_ROLES = {"password text", "password"}
MAX_CHILDREN = 1024
MAX_DEPTH = 32
MAX_DESKTOPS = 8
# Rust rejects any evidence marked truncated; bounded traversal must never
# silently turn an incomplete accessibility tree into a complete shortlist.
truncated = False


def mark_truncated():
    global truncated
    truncated = True


def safe_call(fn, fallback):
    try:
        return fn()
    except Exception:
        return fallback


def child_count(node):
    try:
        count = max(0, int(node.get_child_count()))
    except Exception:
        mark_truncated()
        return 0
    if count > MAX_CHILDREN:
        mark_truncated()
    return min(count, MAX_CHILDREN)


def children(node):
    for index in range(child_count(node)):
        try:
            child = node.get_child_at_index(index)
        except Exception:
            mark_truncated()
            continue
        if child is not None:
            yield index, child
        else:
            mark_truncated()


def role(node):
    return str(safe_call(node.get_role_name, "") or "").strip().lower()


def normalize(value):
    return value.strip().lower().replace("_", " ").replace("-", " ")


def state(node, state_type):
    states = safe_call(node.get_state_set, None)
    return bool(states is not None and safe_call(lambda: states.contains(state_type), False))


def name(node):
    value = safe_call(node.get_name, "")
    return str(value or "")


def rect(node):
    value = safe_call(lambda: node.get_extents(Atspi.CoordType.SCREEN), None)
    if value is None:
        return None
    try:
        return [float(value.x), float(value.y), float(value.width), float(value.height)]
    except Exception:
        return None


def path_nodes(node, path, depth=0, visible_only=False):
    yield path, node
    if depth >= MAX_DEPTH:
        if child_count(node) > 0:
            mark_truncated()
        return
    for index, child in children(node):
        if visible_only and not state(child, Atspi.StateType.SHOWING):
            continue
        yield from path_nodes(child, path + "/" + str(index), depth + 1, visible_only)


def find_windows(app):
    found = []
    for path, node in path_nodes(app, "app"):
        if path == "app":
            continue
        if role(node) in WINDOW_ROLES:
            found.append((path, node))
    return found


def app_nodes():
    try:
        desktop_count = max(0, int(Atspi.get_desktop_count()))
    except Exception:
        mark_truncated()
        desktop_count = 0
    if desktop_count > MAX_DESKTOPS:
        mark_truncated()
    for desktop_index in range(min(desktop_count, MAX_DESKTOPS)):
        try:
            desktop = Atspi.get_desktop(desktop_index)
        except Exception:
            mark_truncated()
            continue
        if desktop is None:
            mark_truncated()
            continue
        for index, app in children(desktop):
            if role(app) == "application":
                yield "desktop/{}/{}".format(desktop_index, index), app


def app_identity(app):
    app_name = name(app)
    process_id = int(safe_call(app.get_process_id, 0) or 0)
    accessible_id = str(safe_call(app.get_accessible_id, "") or "")
    return app_name, process_id, accessible_id


if mode == "browser_viewport":
    # Only geometry from the compositor-selected browser PID and exact page
    # title. Never return document text, attributes, or other applications.
    wanted_pid = int(sys.argv[4])
    matches = []
    for _, app in app_nodes():
        if app_identity(app)[1] != wanted_pid:
            continue
        for _, frame in find_windows(app):
            if name(frame) not in {wanted_window, wanted_window + " - Google Chrome", wanted_window + " - Chromium"}:
                continue
            if not state(frame, Atspi.StateType.SHOWING):
                continue
            for _, node in path_nodes(frame, "frame", visible_only=True):
                if role(node) == "document web" and name(node) == wanted_window:
                    bounds = rect(node)
                    frame_bounds = rect(frame)
                    if bounds and frame_bounds and state(node, Atspi.StateType.SHOWING):
                        matches.append({"process_id": wanted_pid, "frame": frame_bounds, "document": bounds})
    print(json.dumps(matches[0] if len(matches) == 1 and not truncated else {"error": "browser_viewport_not_unique"}))
    raise SystemExit(0)

matching_apps = []
for app_path, app in app_nodes():
    app_name, process_id, accessible_id = app_identity(app)
    if app_name == wanted_application:
        matching_apps.append((app_path, app, process_id, accessible_id))

if len(matching_apps) != 1:
    print(json.dumps({"error": "target_application_not_unique"}))
    raise SystemExit(0)

app_path, app, process_id, accessible_id = matching_apps[0]
windows = find_windows(app)
visible_windows = [
    (path, node) for path, node in windows
    if state(node, Atspi.StateType.SHOWING) or state(node, Atspi.StateType.VISIBLE)
]
if wanted_window:
    windows = [(path, node) for path, node in visible_windows if name(node) == wanted_window]
elif len(visible_windows) == 1:
    windows = visible_windows
else:
    active = [
        (path, node) for path, node in visible_windows
        if state(node, Atspi.StateType.ACTIVE) or state(node, Atspi.StateType.FOCUSED)
    ]
    windows = active if len(active) == 1 else []

if len(windows) != 1:
    print(json.dumps({"error": "target_window_not_unique"}))
    raise SystemExit(0)

window_path, window = windows[0]
window_name = name(window)
window_bounds = rect(window)
window_visible = state(window, Atspi.StateType.SHOWING) or state(window, Atspi.StateType.VISIBLE)
window_focused = state(window, Atspi.StateType.ACTIVE) or state(window, Atspi.StateType.FOCUSED)

if not window_visible or window_bounds is None or (not completion_only and not window_focused):
    print(json.dumps({"error": "target_window_not_actionable"}))
    raise SystemExit(0)

# ACTIVE + SHOWING is the only occlusion assumption this tracer accepts. It
# is recorded in the evidence rather than inferred from a text label.
elements = []
for path, node in path_nodes(window, window_path, visible_only=True):
    if len(elements) >= 512:
        truncated = True
        break
    visible = state(node, Atspi.StateType.SHOWING) or state(node, Atspi.StateType.VISIBLE)
    showing = state(node, Atspi.StateType.SHOWING)
    if not visible or not showing:
        continue
    node_role = role(node)
    if completion_only:
        # Completion is read-only and does not need target focus or geometry.
        # Query only visible, non-editable nodes that can match the caller's
        # exact postcondition; never read a text widget's current value.
        if node_role in PASSWORD_ROLES or node_role in {"entry", "text", "text field"}:
            continue
        if wanted_completion_role and normalize(node_role) != normalize(wanted_completion_role):
            continue
        node_name = name(node)
        if node_name != wanted_completion:
            continue
        bounds = rect(node) or [0.0, 0.0, 0.0, 0.0]
        enabled = state(node, Atspi.StateType.ENABLED)
        editable = False
        protected = False
    else:
        if node_role not in CONTROL_ROLES:
            continue
        bounds = rect(node)
        if bounds is None:
            continue
        enabled = state(node, Atspi.StateType.ENABLED)
        # AT-SPI role is descriptive, not an editability capability.
        editable = state(node, Atspi.StateType.EDITABLE)
        protected = node_role in PASSWORD_ROLES
        # Text widgets can expose their current value through the accessible name;
        # never include it in UI evidence, even when the widget is read-only.
        node_name = "" if protected or node_role in {"entry", "text", "text field"} else name(node)
    elements.append({
        "id": path,
        "role": node_role,
        "name": node_name,
        "x": bounds[0], "y": bounds[1],
        "width": bounds[2], "height": bounds[3],
        "visible": visible,
        "enabled": enabled,
        "showing": showing,
        "focused": state(node, Atspi.StateType.FOCUSED),
        "selected": state(node, Atspi.StateType.SELECTED),
        "checked": state(node, Atspi.StateType.CHECKED),
        "expanded": state(node, Atspi.StateType.EXPANDED),
        "editable": editable,
        "protected": protected,
    })
    if completion_only and len(elements) == 2:
        # Two matches are enough to establish an ambiguous postcondition.
        break

source = {
    "application": app_identity(app)[0],
    "window": window_name,
    "window_id": "{}:{}:{}".format(process_id, app_path, window_path),
    "visible": window_visible,
    "focused": window_focused,
    "occluded": False,
    "source_kind": "native_accessibility",
}
revision_input = json.dumps({"source": source, "elements": elements}, sort_keys=True, separators=(",", ":")).encode()
source["revision"] = hashlib.sha256(revision_input).hexdigest()
# AT-SPI's SCREEN extents do not carry a compositor scale contract. The
# operator must explicitly record a separately verified coordinate mapping;
# otherwise Rust rejects this evidence before input.
coordinate_space = (
    "unknown"
    if completion_only
    else os.environ.get("COMPUTER_USE_ATSPI_COORDINATE_SPACE", "unknown")
)
print(json.dumps({"source": source, "coordinate_space": coordinate_space, "native_frame": window_bounds, "elements": elements, "truncated": truncated}, separators=(",", ":")))
