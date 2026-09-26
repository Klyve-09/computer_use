import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const source = await readFile(new URL("../browser/jev-fast-path/background.js", import.meta.url), "utf8");

function event() {
  const listeners = [];
  return {
    addListener(listener) { listeners.push(listener); },
    emit(...args) { return listeners.map((listener) => listener(...args)); },
  };
}

function deferred() {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
}

function browserHarness() {
  const tabs = new Map([
    [1, { id: 1, windowId: 10, title: "Fixture", active: true, url: "https://fixture.test/" }],
    [2, { id: 2, windowId: 10, title: "Other", active: false, url: "https://other.test/" }],
  ]);
  const calls = [];
  let activeTabId = 1;
  let snapshotRequest = async () => ({ evidence: fixtureEvidence() });
  let port;

  const runtimeMessages = event();
  const tabActivated = event();
  const tabUpdated = event();
  const tabRemoved = event();
  const windowFocusChanged = event();
  const actionClicked = event();
  const portMessages = event();
  const portDisconnect = event();

  const api = {
    runtime: {
      onMessage: runtimeMessages,
      connectNative() {
        port = {
          onMessage: portMessages,
          onDisconnect: portDisconnect,
          postMessage(message) { calls.push({ kind: "native", message }); },
          disconnect() { calls.push({ kind: "disconnect" }); },
        };
        return port;
      },
    },
    tabs: {
      onActivated: tabActivated,
      onUpdated: tabUpdated,
      onRemoved: tabRemoved,
      async get(id) { return { ...tabs.get(id) }; },
      async query() { return [{ ...tabs.get(activeTabId) }]; },
      async sendMessage(id, message) {
        calls.push({ kind: "tab", id, message });
        if (message.type === "snapshot-request") return snapshotRequest();
        return undefined;
      },
    },
    windows: {
      WINDOW_ID_NONE: -1,
      onFocusChanged: windowFocusChanged,
      async get(id) { return { id, focused: true }; },
    },
    scripting: { async executeScript() {} },
    action: { onClicked: actionClicked, async setBadgeText() {} },
  };

  vm.runInNewContext(source, {
    browser: api,
    navigator: { userAgent: "Mozilla/5.0 Firefox/156.0" },
    URL,
    setInterval: () => 1,
    clearInterval: () => {},
  }, { filename: "background.js" });

  async function launch() {
    api.action.onClicked.emit({ ...tabs.get(1), active: true });
    await waitUntil(() => calls.some((call) => call.kind === "tab" && call.message.type === "snapshot"));
  }

  return {
    calls,
    launch,
    get port() { return port; },
    holdSnapshotRequest(barrier) {
      snapshotRequest = () => barrier.promise;
    },
    activateTab(id) {
      activeTabId = id;
      tabs.get(1).active = id === 1;
      tabs.get(2).active = id === 2;
      tabActivated.emit({ tabId: id, windowId: 10 });
    },
    navigate(tabId, status) {
      tabUpdated.emit(tabId, { status });
    },
  };
}

function fixtureEvidence() {
  return {
    source: { focused: true, document_id: "doc-1" },
    browser_viewport: { geometry_verified: true },
    truncated: false,
    elements: [],
  };
}

async function waitUntil(predicate) {
  const deadline = Date.now() + 1000;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error("timed out waiting for background script event");
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
}

async function startRefreshDuringSnapshot(harness, barrier) {
  harness.holdSnapshotRequest(barrier);
  harness.port.onMessage.emit({ type: "refresh", request_id: "request-1" });
  await waitUntil(() => harness.calls.some((call) => call.kind === "tab" && call.message.type === "snapshot-request"));
}

test("a stable active tab forwards its correlated fresh snapshot", async () => {
  const harness = browserHarness();
  await harness.launch();
  const barrier = deferred();
  await startRefreshDuringSnapshot(harness, barrier);

  barrier.resolve({ evidence: fixtureEvidence() });
  await waitUntil(() => harness.calls.some((call) =>
    call.kind === "native" && call.message.type === "evidence"));

  const forwarded = harness.calls.find((call) =>
    call.kind === "native" && call.message.type === "evidence");
  assert.equal(forwarded.message.request_id, "request-1");
  assert.equal(forwarded.message.evidence.source.source_kind, "browser_extension");
  assert.equal(forwarded.message.evidence.source.active_tab, true);
  assert.equal(forwarded.message.evidence.source.browser_origin, "https://fixture.test");
});

test("a tab switch during refresh invalidates the session and discards its late snapshot", async () => {
  const harness = browserHarness();
  await harness.launch();
  const barrier = deferred();
  await startRefreshDuringSnapshot(harness, barrier);
  const invalidationsBeforeSwitch = harness.calls.filter((call) =>
    call.kind === "native" && call.message.type === "invalidate").length;

  harness.activateTab(2);
  barrier.resolve({ evidence: fixtureEvidence() });
  await new Promise((resolve) => setTimeout(resolve, 0));

  assert.equal(
    harness.calls.filter((call) => call.kind === "native" && call.message.type === "invalidate").length
      > invalidationsBeforeSwitch,
    true,
  );
  assert.equal(harness.calls.some((call) => call.kind === "native" && call.message.type === "evidence"), false);
});

test("navigation during refresh invalidates the document and discards its late snapshot", async () => {
  const harness = browserHarness();
  await harness.launch();
  const barrier = deferred();
  await startRefreshDuringSnapshot(harness, barrier);
  const invalidationsBeforeNavigation = harness.calls.filter((call) =>
    call.kind === "native" && call.message.type === "invalidate").length;

  harness.navigate(1, "loading");
  barrier.resolve({ evidence: fixtureEvidence() });
  await new Promise((resolve) => setTimeout(resolve, 0));

  assert.equal(
    harness.calls.filter((call) => call.kind === "native" && call.message.type === "invalidate").length
      > invalidationsBeforeNavigation,
    true,
  );
  assert.equal(harness.calls.some((call) => call.kind === "native" && call.message.type === "evidence"), false);
});
