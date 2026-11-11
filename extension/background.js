// Eddy's browser extension: hands downloads to Eddy (its native messaging
// host, "dev.pah.eddy", which Settings → General → Browser extension sets
// up), with what the browser would have sent with them: its cookies for the
// file, the page it came from and its user agent. From the context menu,
// and (if the options say so) in place of the browser's own downloads.

const HOST = "dev.pah.eddy";

function setUpMenus() {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({ id: "link", title: "Download with Eddy", contexts: ["link"] });
    chrome.contextMenus.create({ id: "media", title: "Download with Eddy", contexts: ["image", "video", "audio"] });
    chrome.contextMenus.create({ id: "page", title: "Download this page with Eddy", contexts: ["page"] });
  });
}

chrome.runtime.onInstalled.addListener(setUpMenus);
chrome.runtime.onStartup.addListener(setUpMenus);

chrome.contextMenus.onClicked.addListener((info, tab) => {
  const url = info.menuItemId === "link" ? info.linkUrl : info.menuItemId === "media" ? info.srcUrl : tab && tab.url;
  const page = (tab && tab.url) || info.pageUrl || "";
  if (url) {
    send({ url, referer: url === page ? "" : page }).catch(() => {});
  }
});

chrome.action.onClicked.addListener(() => chrome.runtime.openOptionsPage());

// Only links Eddy can fetch itself (not blob:, data: or file:).
function fetchable(url) {
  return /^(https?|ftp):/i.test(url || "");
}

// The browser's cookies for `url`, as a Cookie header has them.
async function cookiesFor(url) {
  try {
    const cookies = await chrome.cookies.getAll({ url });
    return cookies.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch (e) {
    return "";
  }
}

// Hands a download to Eddy; throws (and marks the toolbar button) when it
// couldn't.
async function send(download) {
  if (!fetchable(download.url)) {
    throw new Error("Eddy can't fetch this kind of link");
  }
  const message = {
    url: download.url,
    referer: download.referer || "",
    cookies: await cookiesFor(download.url),
    user_agent: navigator.userAgent,
    filename: download.filename || "",
    mime: download.mime || "",
    size: download.size > 0 ? download.size : 0,
  };
  try {
    const answer = await chrome.runtime.sendNativeMessage(HOST, message);
    if (!answer || !answer.ok) {
      throw new Error((answer && answer.error) || "Eddy didn't answer");
    }
    chrome.action.setBadgeText({ text: "" });
    chrome.action.setTitle({ title: "Eddy" });
  } catch (e) {
    chrome.action.setBadgeText({ text: "!" });
    chrome.action.setTitle({ title: `Eddy: ${e.message} (Settings → General → Browser extension, in Eddy)` });
    throw e;
  }
}

// The file's name without its folder (the browser gives a whole path).
function baseName(path) {
  return (path || "").split(/[\\/]/).pop();
}

// The browser's own downloads, if the options say so: paused while Eddy is
// asked, then dropped if it took the file, or carried on if it couldn't.
chrome.downloads.onCreated.addListener(async (item) => {
  const { takeOver } = await chrome.storage.local.get({ takeOver: false });
  const url = item.finalUrl || item.url;
  if (!takeOver || !fetchable(url) || item.state !== "in_progress") {
    return;
  }
  try {
    await chrome.downloads.pause(item.id);
  } catch (e) {
    return;
  }
  try {
    await send({
      url,
      referer: item.referrer,
      filename: baseName(item.filename),
      mime: item.mime,
      size: item.totalBytes > 0 ? item.totalBytes : item.fileSize,
    });
    await chrome.downloads.cancel(item.id);
    await chrome.downloads.erase({ id: item.id });
  } catch (e) {
    chrome.downloads.resume(item.id).catch(() => {});
  }
});
