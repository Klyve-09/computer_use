(() => {
  if (globalThis.__computerUseEvidenceInstalled) return;
  globalThis.__computerUseEvidenceInstalled = true;
  const ids = new WeakMap();
  let nextId = 0;
  let timer = null;
  let observer = null;
  let heartbeat = null;
  let listenersInstalled = false;
  let enabled = false;
  const videoTimes = new WeakMap();
  const documentId = crypto.randomUUID();
  const api = globalThis.browser ?? globalThis.chrome;

  function allowedVisible(element, clipped = false) {
    for (let node = element; node && node.nodeType === Node.ELEMENT_NODE; node = node.parentElement) {
      if (node.hidden || node.inert || node.getAttribute("aria-hidden") === "true") return false;
      const style = getComputedStyle(node);
      if (style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse"
          || Number(style.opacity) === 0) return false;
    }
    const rect = element.getBoundingClientRect();
    if (clipped) return rect.width > 0 && rect.height > 0
      && rect.right > 0 && rect.bottom > 0 && rect.left < innerWidth && rect.top < innerHeight;
    return rect.width > 0 && rect.height > 0
      && rect.left >= 0 && rect.top >= 0
      && rect.right <= innerWidth && rect.bottom <= innerHeight;
  }

  function identifier(element) {
    if (!ids.has(element)) ids.set(element, "tab-" + (++nextId));
    return ids.get(element);
  }

  function visibleText(element) {
    const text = (element.getAttribute("aria-label") || element.innerText || "")
      .replace(/[\u0000-\u001f\u007f]/g, " ").replace(/\s+/g, " ").trim();
    return text.slice(0, 120);
  }

  function editableLabel(element) {
    const values = [
      element.getAttribute("aria-label"),
      element.getAttribute("placeholder"),
      element.getAttribute("title")
    ];
    return (values.find((value) => value && value.trim()) || "")
      .replace(/[\u0000-\u001f\u007f]/g, " ").replace(/\s+/g, " ").trim().slice(0, 120);
  }

  function isSafeTextEntry(element) {
    if (element.disabled || element.readOnly || element.getAttribute("aria-disabled") === "true") return false;
    if (element instanceof HTMLInputElement) {
      if (!["text", "search", "email", "url", "tel", "number"].includes(element.type.toLowerCase())) return false;
    } else if (!(element instanceof HTMLTextAreaElement) && !element.isContentEditable) return false;
    const autocomplete = (element.getAttribute("autocomplete") || "").toLowerCase();
    return !/(password|one-time-code|cc-|current-password|new-password)/.test(autocomplete);
  }

  function currentSnapshot() {
    if (document.visibilityState !== "visible" || !document.hasFocus()) return null;
    // Firefox exposes the viewport origin: screen-relative on XWayland and
    // window-relative on native Wayland. Chromium screenX/screenY locate the
    // outer browser window, so leave its geometry unverified.
    const viewportX = window.mozInnerScreenX;
    const viewportY = window.mozInnerScreenY;
    const dpr = window.devicePixelRatio;
    const visualScale = window.visualViewport?.scale ?? 1;
    const geometryVerified = typeof viewportX === "number" && Number.isFinite(viewportX)
      && typeof viewportY === "number" && Number.isFinite(viewportY)
      && Number.isFinite(dpr) && visualScale === 1;
    const elements = [];
    const netflix = globalThis.location?.origin === "https://www.netflix.com";
    const playerPage = netflix && /^\/watch\/\d+$/.test(location.pathname);
    const mediaSelector = netflix
      ? ', a.slider-refocus, a[data-uia="play-button"], button[data-uia="player-play-pause"], video'
      : '';
    const selector = '[role="tab"], [role="status"], [aria-live], input, textarea, [contenteditable="true"], [role="textbox"]' + mediaSelector;
    for (const element of document.querySelectorAll(selector)) {
      if (!allowedVisible(element, netflix && element.tagName === "VIDEO")) continue;
      const textEntry = isSafeTextEntry(element);
      if ((element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement
          || element.isContentEditable || element.getAttribute("role") === "textbox") && !textEntry) continue;
      let mediaRole = null;
      if (netflix && element.tagName === "A") {
        const url = new URL(element.href, location.href);
        if (url.origin === location.origin && /^\/watch\/\d+$/.test(url.pathname)
            && element.getAttribute("data-uia") === "play-button") mediaRole = "media play";
        else if (url.origin === location.origin && url.pathname === "/browse"
            && /^\d+$/.test(url.searchParams.get("jbv") || "")
            && element.matches("a.slider-refocus")) mediaRole = "media title";
      } else if (playerPage && element.tagName === "BUTTON"
          && element.getAttribute("data-uia") === "player-play-pause") {
        const video = document.querySelector("video");
        if (video && video.paused && !video.ended) mediaRole = "media play";
      } else if (playerPage && element.tagName === "VIDEO") {
        const previous = videoTimes.get(element);
        const now = performance.now();
        const advancedAt = previous && element.currentTime > previous.time
          ? now : previous?.advancedAt;
        videoTimes.set(element, { time: element.currentTime, advancedAt });
        if (element.paused || element.ended || element.readyState < 2
            || advancedAt === undefined || now - advancedAt > 1000) continue;
        mediaRole = "status";
      }
      if (element.tagName === "VIDEO" && !mediaRole) continue;
      if (netflix && ["A", "BUTTON", "VIDEO"].includes(element.tagName) && !mediaRole
          && !element.getAttribute("role")) continue;
      const role = mediaRole || (textEntry ? "text field"
        : element.getAttribute("role") === "tab" ? "tab"
        : element.getAttribute("role") === "status" ? "status" : "label");
      if (role === "tab") {
        const panel = element.getAttribute("aria-controls");
        const controlledPanel = panel && document.getElementById(panel);
        if (element.hasAttribute("href") || element.closest("a")
            || (element.tagName === "BUTTON" && element.type === "submit")
            || !controlledPanel || controlledPanel.getAttribute("role") !== "tabpanel") continue;
      }
      const name = mediaRole === "status" ? "Media playback active"
        : mediaRole === "media play" ? "Play " + document.title.slice(0, 100)
        : mediaRole === "media title" ? (visibleText(element) || element.querySelector("img")?.alt || "").slice(0, 120)
        : textEntry ? editableLabel(element) : visibleText(element);
      if (!name) continue;
      const bounds = element.getBoundingClientRect();
      const rect = mediaRole === "status" ? {
        left: Math.max(0, bounds.left), top: Math.max(0, bounds.top),
        width: Math.min(innerWidth, bounds.right) - Math.max(0, bounds.left),
        height: Math.min(innerHeight, bounds.bottom) - Math.max(0, bounds.top)
      } : bounds;
      const center = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
      if (mediaRole !== "status" && (!center || (center !== element && !element.contains(center)))) continue;
      elements.push({
        id: identifier(element),
        role,
        name,
        x: rect.left,
        y: rect.top,
        width: rect.width,
        height: rect.height,
        visible: true,
        enabled: !element.disabled && element.getAttribute("aria-disabled") !== "true",
        showing: true,
        focused: document.activeElement === element,
        selected: element.getAttribute("aria-selected") === "true",
        editable: textEntry,
        protected: false
      });
      if (elements.length === 512) break;
    }
    const truncated = document.querySelectorAll(selector).length > 512;
    return {
      source: {
        application: "",
        window: document.title.slice(0, 256),
        window_id: "",
        revision: documentId,
        visible: true,
        focused: document.hasFocus(),
        occluded: false,
        source_kind: "browser_extension",
        active_tab: true,
        browser_window_id: "",
        browser_tab_id: "",
        document_id: documentId,
        geometry_verified: geometryVerified
      },
      coordinate_space: "browser_viewport_css",
      browser_viewport: {
        screen_x: geometryVerified ? viewportX : 0,
        screen_y: geometryVerified ? viewportY : 0,
        width: innerWidth,
        height: innerHeight,
        device_pixel_ratio: dpr,
        visual_scale: visualScale,
        geometry_verified: geometryVerified
      },
      captured_at_unix_ms: Date.now(),
      truncated,
      elements
    };
  }

  function publish() {
    timer = null;
    if (!enabled) return;
    const evidence = currentSnapshot();
    if (!evidence) {
      void api.runtime.sendMessage({ type: "inactive" });
      return;
    }
    void api.runtime.sendMessage({ type: "evidence", evidence });
  }

  function schedule() {
    if (!enabled) return;
    if (timer !== null) clearTimeout(timer);
    timer = setTimeout(publish, 120);
  }

  api.runtime.onMessage.addListener((message, _sender, sendResponse) => {
    if (message?.type === "stop") {
      enabled = false;
      if (timer !== null) clearTimeout(timer);
      if (heartbeat !== null) clearInterval(heartbeat);
      observer?.disconnect();
      observer = null;
      heartbeat = null;
      return undefined;
    }
    if (message?.type !== "snapshot" && message?.type !== "snapshot-request") return undefined;
    enabled = true;
    if (!observer && document.documentElement) {
      observer = new MutationObserver(schedule);
      observer.observe(document.documentElement, {
        subtree: true,
        childList: true,
        attributes: true,
        characterData: true
      });
      if (!listenersInstalled) {
        addEventListener("resize", schedule);
        addEventListener("focus", schedule);
        addEventListener("blur", schedule);
        document.addEventListener("visibilitychange", schedule);
        listenersInstalled = true;
      }
      heartbeat = setInterval(schedule, 1000);
    }
    if (message.type === "snapshot-request") {
      sendResponse({ evidence: currentSnapshot() });
      return true;
    }
    // Let the browser action popup close before taking the initial snapshot.
    schedule();
    return undefined;
  });
})();
