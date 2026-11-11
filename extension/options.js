// The extension's one option: the browser's downloads go to Eddy.
const box = document.getElementById("takeOver");
chrome.storage.local.get({ takeOver: false }).then(({ takeOver }) => {
  box.checked = takeOver;
});
box.addEventListener("change", () => chrome.storage.local.set({ takeOver: box.checked }));
