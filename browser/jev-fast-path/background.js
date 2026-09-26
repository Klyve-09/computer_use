const api = globalThis.browser ?? globalThis.chrome;
const NATIVE_HOST = "ai.typesafe.computer_use";
const activeTab = { id: null, port: null, application: null, documentReady: false, poller: null };

function browserName() {
  const ua = navigator.userAgent;
  if (ua.includes("Firefox/")) return "Firefox";
  if (ua.includes("Chrome/") || ua.includes("Chromium/")) return "Chrome";
  return null;
}

function stopSession(invalidate = true) {
  const previousTabId = activeTab.id;
  if (activeTab.poller !== null) clearInterval(activeTab.poller);
  activeTab.poller = null;
  if (previousTabId !== null) {
    void api.tabs.sendMessage(previousTabId, { type: "stop" }).catch(() => {});
  }
  if (activeTab.port && invalidate) {
    try { activeTab.port.postMessage({ type: "invalidate" }); } catch (_) {}
  }
  try { activeTab.port?.disconnect(); } catch (_) {}
  if (previousTabId !== null) api.action.setBadgeText({ tabId: previousTabId, text: "" });
  activeTab.id = null;
  activeTab.port = null;
  activeTab.application = null;
  activeTab.documentReady = false;
}

async function launchForTab(tab) {
  if (tab?.id === undefined || tab?.id === null || !tab.active) return;
  const app = browserName();
  if (!app) return;
  stopSession();
  try {
    const port = api.runtime.connectNative(NATIVE_HOST);
    activeTab.id = tab.id;
    activeTab.port = port;
    activeTab.application = app;
    activeTab.documentReady = false;
    port.onDisconnect.addListener(() => {
      if (activeTab.port === port) stopSession(false);
    });
    port.onMessage.addListener((response) => {
      if (response?.type === "refresh" && activeTab.port === port) {
        void refreshEvidence(response.request_id, tab.id, port, app);
        return;
      }
      if (response?.ok === false && activeTab.port === port) stopSession();
    });
    activeTab.poller = setInterval(() => {
      if (activeTab.id === tab.id && activeTab.port === port) {
        try { port.postMessage({ type: "poll" }); } catch (_) { stopSession(false); }
      }
    }, 100);
    api.action.setBadgeText({ tabId: tab.id, text: "ON" });
    await api.scripting.executeScript({ target: { tabId: tab.id }, files: ["content.js"] });
    if (activeTab.id !== tab.id || activeTab.port !== port) return;
    activeTab.documentReady = true;
    await api.tabs.sendMessage(tab.id, { type: "snapshot" });
  } catch (_) {
    stopSession();
  }
}

api.action.onClicked.addListener((tab) => {
  if (activeTab.id === tab?.id) stopSession();
  else void launchForTab(tab);
});

async function handleEvidence(message, sender, port, application, requestId) {
  const senderTabId = sender.tab.id;
  try {
    const tab = await api.tabs.get(senderTabId);
    const window = await api.windows.get(tab.windowId);
    const pageUrl = new URL(sender.url);
    if (activeTab.id !== senderTabId || activeTab.port !== port || !activeTab.documentReady) return;
    if (!tab.active || !window.focused || !["https:", "http:"].includes(pageUrl.protocol)
        || pageUrl.origin !== new URL(tab.url).origin) {
      stopSession();
      return;
    }
    const snapshot = message.evidence;
    if (!snapshot || snapshot.truncated || !Array.isArray(snapshot.elements)
        || snapshot.elements.length > 512 || snapshot.source?.focused !== true) {
      stopSession();
      return;
    }
    snapshot.source.application = application;
    snapshot.source.window = String(tab.title || "").slice(0, 256);
    snapshot.source.window_id = String(tab.windowId) + ":" + String(tab.id) + ":" + snapshot.source.document_id;
    snapshot.source.browser_window_id = String(tab.windowId);
    snapshot.source.browser_tab_id = String(tab.id);
    snapshot.source.source_kind = "browser_extension";
    snapshot.source.browser_origin = pageUrl.origin;
    snapshot.source.active_tab = true;
    snapshot.source.visible = true;
    snapshot.source.focused = true;
    snapshot.source.occluded = false;
    snapshot.source.geometry_verified = snapshot.browser_viewport?.geometry_verified === true;
    snapshot.source.revision = snapshot.source.document_id;
    delete snapshot.browser_viewport.geometry_verified;
    snapshot.captured_at_unix_ms = Date.now();
    port.postMessage(requestId
      ? { type: "evidence", request_id: requestId, evidence: snapshot }
      : { type: "evidence", evidence: snapshot });
  } catch (_) {
    if (activeTab.port === port) stopSession();
  }
}

async function refreshEvidence(requestId, tabId, port, application) {
  if (typeof requestId !== "string" || requestId.length > 64
      || activeTab.id !== tabId || activeTab.port !== port || !activeTab.documentReady) return;
  try {
    let tab = await api.tabs.get(tabId);
    let window = await api.windows.get(tab.windowId);
    let active = await api.tabs.query({ active: true, windowId: tab.windowId });
    if (activeTab.id !== tabId || activeTab.port !== port
        || active[0]?.id !== tabId || !window.focused) {
      if (activeTab.port === port) stopSession();
      return;
    }
    const response = await api.tabs.sendMessage(tabId, { type: "snapshot-request" });
    tab = await api.tabs.get(tabId);
    window = await api.windows.get(tab.windowId);
    active = await api.tabs.query({ active: true, windowId: tab.windowId });
    if (activeTab.id !== tabId || activeTab.port !== port
        || active[0]?.id !== tabId || !window.focused || !response?.evidence) {
      if (activeTab.port === port) stopSession();
      return;
    }
    await handleEvidence(
      { evidence: response.evidence },
      { tab, url: tab.url, frameId: 0 },
      port,
      application,
      requestId
    );
  } catch (_) {
    if (activeTab.port === port) stopSession();
  }
}

api.runtime.onMessage.addListener((message, sender) => {
  if (message?.type === "inactive" && sender.tab?.id === activeTab.id) {
    stopSession();
    return undefined;
  }
  if (message?.type !== "evidence" || !sender.tab?.id || sender.frameId !== 0
      || sender.tab.id !== activeTab.id || !activeTab.port || !activeTab.documentReady) return undefined;
  void handleEvidence(message, sender, activeTab.port, activeTab.application);
  return undefined;
});

api.tabs.onActivated.addListener(({ tabId }) => {
  if (tabId !== activeTab.id) stopSession();
});

api.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (tabId !== activeTab.id) return;
  if (changeInfo.status === "loading") {
    activeTab.documentReady = false;
    void api.tabs.sendMessage(tabId, { type: "stop" }).catch(() => {});
    try { activeTab.port?.postMessage({ type: "invalidate" }); } catch (_) {}
  } else if (changeInfo.status === "complete") {
    const port = activeTab.port;
    activeTab.documentReady = false;
    try { port?.postMessage({ type: "invalidate" }); } catch (_) {}
    void api.scripting.executeScript({ target: { tabId }, files: ["content.js"] })
      .then(() => {
        if (activeTab.id !== tabId || activeTab.port !== port) return;
        activeTab.documentReady = true;
        return api.tabs.sendMessage(tabId, { type: "snapshot" });
      })
      .catch(() => {
        if (activeTab.port === port) stopSession();
      });
  }
});

api.tabs.onRemoved.addListener((tabId) => {
  if (tabId === activeTab.id) stopSession();
});

api.windows.onFocusChanged.addListener((windowId) => {
  if (windowId === api.windows.WINDOW_ID_NONE) {
    stopSession();
    return;
  }
  const tabId = activeTab.id;
  if (tabId === null) return;
  void api.tabs.get(tabId).then((tab) => {
    if (activeTab.id === tabId && tab.windowId !== windowId) stopSession();
  }).catch(() => {
    if (activeTab.id === tabId) stopSession();
  });
});
