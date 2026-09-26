import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const source = await readFile(new URL("../browser/jev-fast-path/content.js", import.meta.url), "utf8");

function snapshotWithOpacity(tabOpacity, parentOpacity) {
  let onMessage;
  let response;
  const parent = {
    nodeType: 1,
    hidden: false,
    inert: false,
    parentElement: null,
    getAttribute: () => null,
  };
  const tab = {
    nodeType: 1,
    tagName: "BUTTON",
    type: "button",
    innerText: "Details",
    disabled: false,
    isContentEditable: false,
    hidden: false,
    inert: false,
    parentElement: parent,
    getAttribute(name) {
      return ({
        role: "tab",
        "aria-controls": "details-panel",
        "aria-label": null,
        "aria-selected": "false",
        "aria-hidden": null,
      })[name] ?? null;
    },
    hasAttribute: () => false,
    closest: () => null,
    contains: (node) => node === tab,
    getBoundingClientRect: () => ({ left: 10, top: 10, right: 90, bottom: 40, width: 80, height: 30 }),
  };
  const panel = { getAttribute: (name) => name === "role" ? "tabpanel" : null };
  const browser = {
    runtime: {
      onMessage: { addListener(listener) { onMessage = listener; } },
      sendMessage() {},
    },
  };
  class MutationObserver {
    observe() {}
    disconnect() {}
  }
  const document = {
    title: "Fixture",
    visibilityState: "visible",
    documentElement: {},
    hasFocus: () => true,
    querySelectorAll: () => [tab],
    getElementById: () => panel,
    addEventListener() {},
    elementFromPoint: () => tab,
  };

  vm.runInNewContext(source, {
    browser,
    document,
    crypto: { randomUUID: () => "document-1" },
    Node: { ELEMENT_NODE: 1 },
    HTMLInputElement: class HTMLInputElement {},
    HTMLTextAreaElement: class HTMLTextAreaElement {},
    MutationObserver,
    window: { mozInnerScreenX: 0, mozInnerScreenY: 0, devicePixelRatio: 1, visualViewport: { scale: 1 } },
    innerWidth: 800,
    innerHeight: 600,
    getComputedStyle: (node) => ({
      display: "block",
      visibility: "visible",
      opacity: node === tab ? String(tabOpacity) : String(parentOpacity),
    }),
    addEventListener() {},
    setInterval: () => 1,
    clearInterval() {},
    setTimeout: () => 1,
    clearTimeout() {},
  }, { filename: "content.js" });

  onMessage({ type: "snapshot-request" }, {}, (value) => { response = value; });
  return response.evidence;
}

test("fully transparent tabs are excluded from browser evidence", () => {
  assert.equal(snapshotWithOpacity(0, 1).elements.length, 0);
  assert.equal(snapshotWithOpacity(1, 0).elements.length, 0);
  assert.equal(snapshotWithOpacity(1, 1).elements.length, 1);
});
