const BRIDGE_URL = "http://127.0.0.1:38933/update";
const FETCH_TIMEOUT_MS = 2000;
const HEARTBEAT_ALARM = "bridge-heartbeat";
const HEARTBEAT_PERIOD_MINUTES = 0.5;
const INITIAL_RETRY_DELAY_MS = 1000;
const MAX_RETRY_DELAY_MS = 30000;

const FORCED_PUSH_REASONS = new Set([
  "window-focus-changed",
  "heartbeat",
  "extension-installed",
  "runtime-startup"
]);

let pendingReason = null;
let pendingForced = false;
let flushInFlight = false;
let retryTimerId = null;
let retryDelayMs = INITIAL_RETRY_DELAY_MS;
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

async function pushSnapshotPayload(reason, snapshot, forced) {
  const fingerprint = stableFingerprint(snapshot);
  const fingerprintKey = String(snapshot.chrome_window.id);
  if (!forced && fingerprint === lastPayloadFingerprints.get(fingerprintKey)) {
    return true;
  }

  const payload = {
    schema: "org.imalison.chrome_window_info.v1",
    event_reason: reason,
    event_time: new Date().toISOString(),
    ...snapshot
  };

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

    const result = await response.json().catch(() => ({}));
    if (result.mapped === false && result.hyprland !== false) {
      // The bridge has a Hyprland backend but could not map this window yet;
      // leaving the fingerprint unset makes the next event or heartbeat retry.
      lastPayloadFingerprints.delete(fingerprintKey);
    } else {
      lastPayloadFingerprints.set(fingerprintKey, fingerprint);
    }
    return true;
  } catch (error) {
    // The bridge may have processed the request even though the response was
    // lost, so the consumer's state is unknown; forget the fingerprint to
    // force the next push through.
    lastPayloadFingerprints.delete(fingerprintKey);
    console.warn("Failed to publish Chrome window snapshot", error);
    return false;
  } finally {
    clearTimeout(timeoutId);
  }
}

async function pushAllWindowSnapshots(reason, forced = false) {
  const windows = await chromeCall((getInfo, callback) => {
    chrome.windows.getAll(getInfo, callback);
  }, {
    populate: true,
    windowTypes: ["normal", "popup", "app"]
  });

  let allSucceeded = true;
  for (const chromeWindow of windows) {
    const tab = chromeWindow.tabs?.find((candidate) => candidate.active);
    if (tab) {
      const succeeded = await pushSnapshotPayload(
        reason,
        snapshotFromTabAndWindow(tab, chromeWindow),
        forced
      );
      allSucceeded = allSucceeded && succeeded;
    }
  }
  return allSucceeded;
}

function scheduleRetry() {
  if (retryTimerId !== null) {
    return;
  }

  retryTimerId = setTimeout(() => {
    retryTimerId = null;
    schedulePush("retry");
  }, retryDelayMs);
  retryDelayMs = Math.min(retryDelayMs * 2, MAX_RETRY_DELAY_MS);
}

async function flushPendingPushes() {
  if (flushInFlight) {
    return;
  }

  flushInFlight = true;

  try {
    while (pendingReason !== null) {
      const reasonToSend = pendingReason;
      const forcedToSend = pendingForced;
      pendingReason = null;
      pendingForced = false;

      let succeeded = false;
      try {
        succeeded = await pushAllWindowSnapshots(reasonToSend, forcedToSend);
      } catch (error) {
        console.warn("Failed to push Chrome window snapshots", error);
      }

      if (succeeded) {
        retryDelayMs = INITIAL_RETRY_DELAY_MS;
      } else {
        scheduleRetry();
      }
    }
  } finally {
    flushInFlight = false;
  }
}

function schedulePush(reason) {
  pendingReason = reason;
  if (FORCED_PUSH_REASONS.has(reason)) {
    pendingForced = true;
  }
  void flushPendingPushes();
}

async function ensureHeartbeatAlarm() {
  const existing = await chrome.alarms.get(HEARTBEAT_ALARM);
  if (!existing) {
    chrome.alarms.create(HEARTBEAT_ALARM, {
      periodInMinutes: HEARTBEAT_PERIOD_MINUTES
    });
  }
}

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === HEARTBEAT_ALARM) {
    schedulePush("heartbeat");
  }
});

chrome.tabs.onActivated.addListener(() => {
  schedulePush("tab-activated");
});

chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  if (!tab.active) {
    return;
  }

  if (changeInfo.status || changeInfo.title || changeInfo.url || changeInfo.favIconUrl) {
    schedulePush("tab-updated");
  }
});

chrome.windows.onFocusChanged.addListener((windowId) => {
  if (windowId === chrome.windows.WINDOW_ID_NONE) {
    schedulePush("window-focus-cleared");
    return;
  }

  schedulePush("window-focus-changed");
});

chrome.runtime.onInstalled.addListener(() => {
  schedulePush("extension-installed");
});

chrome.runtime.onStartup.addListener(() => {
  schedulePush("runtime-startup");
});

void ensureHeartbeatAlarm();

globalThis.chromeFaviconBridgeDebug = {
  pushAll: (reason = "manual-debug") => pushAllWindowSnapshots(reason, true),
  state: () => ({
    flushInFlight,
    pendingReason,
    pendingForced,
    retryDelayMs,
    lastPayloadFingerprints: Object.fromEntries(lastPayloadFingerprints)
  })
};
