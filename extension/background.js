const BRIDGE_URL = "http://127.0.0.1:38933/update";
const FETCH_TIMEOUT_MS = 2000;

let pendingReason = null;
let flushInFlight = false;
const lastPayloadFingerprints = new Map();

function chromeCall(apiFunction, ...args) {
  return new Promise((resolve, reject) => {
    apiFunction(...args, (result) => {
      const error = chrome.runtime.lastError;
      if (error) {
        reject(new Error(error.message));
      } else {
        resolve(result);
      }
    });
  });
}

function stableFingerprint(payload) {
  return JSON.stringify(payload);
}

function snapshotFromTabAndWindow(tab, chromeWindow) {
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

async function getActiveTabSnapshot(windowIdHint = null) {
  const query = { active: true };

  if (windowIdHint !== null && windowIdHint !== chrome.windows.WINDOW_ID_NONE) {
    query.windowId = windowIdHint;
  } else {
    query.lastFocusedWindow = true;
  }

  const [tab] = await chromeCall((queryInfo, callback) => {
    chrome.tabs.query(queryInfo, callback);
  }, query);
  if (!tab) {
    return null;
  }

  const chromeWindow = await chromeCall((windowId, callback) => {
    chrome.windows.get(windowId, callback);
  }, tab.windowId);

  return snapshotFromTabAndWindow(tab, chromeWindow);
}

async function pushSnapshotPayload(reason, snapshot) {
  const payload = {
    schema: "org.imalison.chrome_window_info.v1",
    event_reason: reason,
    event_time: new Date().toISOString(),
    ...snapshot
  };

  const fingerprint = stableFingerprint(snapshot);
  const fingerprintKey = String(snapshot.chrome_window.id);
  if (
    fingerprint === lastPayloadFingerprints.get(fingerprintKey) &&
    reason !== "window-focus-changed"
  ) {
    return;
  }

  const controller = new AbortController();
  const timeoutId = setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS);

  try {
    const response = await fetch(BRIDGE_URL, {
      method: "POST",
      headers: {
        "Content-Type": "application/json"
      },
      body: JSON.stringify(payload),
      keepalive: true,
      signal: controller.signal
    });

    if (!response.ok) {
      throw new Error(`Bridge returned HTTP ${response.status}`);
    }

    lastPayloadFingerprints.set(fingerprintKey, fingerprint);
  } catch (error) {
    console.warn("Failed to publish Chrome window snapshot", error);
  } finally {
    clearTimeout(timeoutId);
  }
}

async function pushSnapshot(reason, windowIdHint = null) {
  const snapshot = await getActiveTabSnapshot(windowIdHint);
  if (snapshot) {
    await pushSnapshotPayload(reason, snapshot);
  }
}

async function pushAllWindowSnapshots(reason) {
  const windows = await chromeCall((getInfo, callback) => {
    chrome.windows.getAll(getInfo, callback);
  }, {
    populate: true,
    windowTypes: ["normal"]
  });

  for (const chromeWindow of windows) {
    const tab = chromeWindow.tabs?.find((candidate) => candidate.active);
    if (tab) {
      await pushSnapshotPayload(reason, snapshotFromTabAndWindow(tab, chromeWindow));
    }
  }
}

async function flushPendingPushes() {
  if (flushInFlight) {
    return;
  }

  flushInFlight = true;

  try {
    while (pendingReason !== null) {
      const reasonToSend = pendingReason;
      pendingReason = null;

      try {
        await pushAllWindowSnapshots(reasonToSend);
      } catch (error) {
        console.warn("Failed to push Chrome window snapshots", error);
      }
    }
  } finally {
    flushInFlight = false;
  }
}

function schedulePush(reason) {
  pendingReason = reason;
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

globalThis.chromeFaviconBridgeDebug = {
  pushAll: (reason = "manual-debug") => pushAllWindowSnapshots(reason),
  state: () => ({
    flushInFlight,
    pendingReason,
    lastPayloadFingerprints: Object.fromEntries(lastPayloadFingerprints)
  })
};
