const BRIDGE_URL = "http://127.0.0.1:38933/update";

let pendingReason = null;
let pendingWindowHint = null;
let flushInFlight = false;
let lastPayloadFingerprint = "";

function stableFingerprint(payload) {
  return JSON.stringify(payload);
}

async function getActiveTabSnapshot(windowIdHint = null) {
  const query = { active: true };

  if (windowIdHint !== null && windowIdHint !== chrome.windows.WINDOW_ID_NONE) {
    query.windowId = windowIdHint;
  } else {
    query.lastFocusedWindow = true;
  }

  const [tab] = await chrome.tabs.query(query);
  if (!tab) {
    return null;
  }

  const chromeWindow = await chrome.windows.get(tab.windowId);

  return {
    tab: {
      id: tab.id,
      window_id: tab.windowId,
      index: tab.index,
      title: tab.title ?? "",
      url: tab.url ?? "",
      favicon_url: tab.favIconUrl ?? "",
      audible: Boolean(tab.audible),
      muted: Boolean(tab.mutedInfo?.muted),
      discarded: Boolean(tab.discarded),
      status: tab.status ?? ""
    },
    chrome_window: {
      id: chromeWindow.id,
      focused: Boolean(chromeWindow.focused),
      state: chromeWindow.state ?? "normal",
      type: chromeWindow.type ?? "normal",
      top: chromeWindow.top ?? null,
      left: chromeWindow.left ?? null,
      width: chromeWindow.width ?? null,
      height: chromeWindow.height ?? null
    }
  };
}

async function pushSnapshot(reason, windowIdHint = null) {
  const snapshot = await getActiveTabSnapshot(windowIdHint);
  if (!snapshot) {
    return;
  }

  const payload = {
    schema: "org.imalison.chrome_window_info.v1",
    event_reason: reason,
    event_time: new Date().toISOString(),
    ...snapshot
  };

  const fingerprint = stableFingerprint(snapshot);
  if (fingerprint === lastPayloadFingerprint && reason !== "window-focus-changed") {
    return;
  }

  lastPayloadFingerprint = fingerprint;

  try {
    await fetch(BRIDGE_URL, {
      method: "POST",
      headers: {
        "Content-Type": "application/json"
      },
      body: JSON.stringify(payload),
      keepalive: true
    });
  } catch (_error) {
    // The bridge daemon is optional and may not be running.
  }
}

async function flushPendingPushes() {
  if (flushInFlight) {
    return;
  }

  flushInFlight = true;

  while (pendingReason !== null) {
    const reasonToSend = pendingReason;
    const hintToSend = pendingWindowHint;
    pendingReason = null;
    pendingWindowHint = null;

    await pushSnapshot(reasonToSend, hintToSend);
  }

  flushInFlight = false;
}

function schedulePush(reason, windowIdHint = null) {
  pendingReason = reason;
  pendingWindowHint = windowIdHint;
  void flushPendingPushes();
}

chrome.tabs.onActivated.addListener(({ windowId }) => {
  schedulePush("tab-activated", windowId);
});

chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  if (!tab.active) {
    return;
  }

  if (changeInfo.status || changeInfo.title || changeInfo.url || changeInfo.favIconUrl) {
    schedulePush("tab-updated", tab.windowId);
  }
});

chrome.windows.onFocusChanged.addListener((windowId) => {
  if (windowId === chrome.windows.WINDOW_ID_NONE) {
    schedulePush("window-focus-cleared", null);
    return;
  }

  schedulePush("window-focus-changed", windowId);
});

chrome.runtime.onInstalled.addListener(() => {
  schedulePush("extension-installed");
});

chrome.runtime.onStartup.addListener(() => {
  schedulePush("runtime-startup");
});
