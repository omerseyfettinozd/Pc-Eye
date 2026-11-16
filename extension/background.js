"use strict";

// ---- Sabitler ----
const NATIVE_APP = "pc_eye_bridge";
const RECONNECT_MS = 2000;
const DEFAULT_TIMEOUT_MS = 10000;
const NAVIGATE_TIMEOUT_MS = 20000;
const NAV_WAIT_MS = 5000;
const LOAD_WAIT_MS = 18000;
const MAX_CONSOLE = 200;
const MAX_DIALOGS = 50;
const MAX_NETWORK = 300;
const MAX_REQ_MAP = 1000;

const BUILTIN_DENY = [
  "bank", "banka", "paypal", "wise.com", "binance", "coinbase", "1password",
  "bitwarden", "lastpass", "keepass", "e-devlet", "turkiye.gov.tr",
  "accounts.google.com", "login.microsoftonline.com"
];

const PAGE_OPS = new Set([
  "snapshot", "click", "type", "select", "check", "hover", "focus", "key",
  "scroll", "text", "rect", "wait", "upload"
]);
// Gezinme başlatabilen eylemler: yanıt kaybolursa background sonucu üretir.
const ACTION_OPS = new Set(["click", "type", "select", "check", "hover", "focus", "key", "upload"]);

// ---- Durum ----
let port = null;
let reconnectTimer = null;
let extraDeny = [];
let allowList = [];
const navSeq = new Map();      // sekme -> ana çerçeve gezinme sayacı
const navPending = new Map();  // sekme -> gezinme sürüyor mu
let eventSeq = 0;
// Varsayılan ret: ajan görmeden yıkıcı bir onay sorusu kabul edilmesin.
let dialogPolicy = { accept: false, text: null };
const consoleBuf = new Map();  // sekme -> konsol olayları
const dialogBuf = new Map();   // sekme -> diyalog olayları
const networkBuf = new Map();  // sekme -> ağ istekleri
const tabOrigins = new Map();  // sekme -> son ana çerçeve kaynağı
const tabUrls = new Map();     // sekme -> bilinen son URL
const reqMap = new Map();      // requestId -> { tabId, entry, t0 }

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function errText(e) {
  if (e && typeof e === "object" && e.message) return String(e.message);
  return String(e);
}

function numOr(v, def) {
  if (v === undefined || v === null || v === "") return def;
  const n = Number(v);
  return Number.isFinite(n) ? n : def;
}

function post(p, obj) {
  try {
    p.postMessage(obj);
  } catch (e) {
    console.warn("pc-eye: ana makineye yazılamadı:", errText(e));
  }
}

function originOf(url) {
  try {
    const u = new URL(url);
    if (u.origin && u.origin !== "null") return u.origin.toLowerCase();
    return (u.protocol + "//" + u.host).toLowerCase();
  } catch (e) {
    return "";
  }
}

function pushRing(map, tabId, entry, maxLen) {
  let arr = map.get(tabId);
  if (!arr) {
    arr = [];
    map.set(tabId, arr);
  }
  arr.push(entry);
  if (arr.length > maxLen) arr.splice(0, arr.length - maxLen);
}

function lastSeqOf(map, tabId) {
  const arr = map.get(tabId);
  return arr && arr.length ? arr[arr.length - 1].seq : 0;
}

function formatDialog(d) {
  const q = JSON.stringify(d.message || "");
  if (d.type === "alert") return "alert " + q;
  return d.type + " " + q + " -> " + JSON.stringify(d.answer === undefined ? null : d.answer);
}

function formatNet(e) {
  const st = e.error ? ("ERR(" + e.error + ")") : (e.status !== null ? String(e.status) : "...");
  const ms = e.ms !== null ? (" " + e.ms + "ms") : "";
  return e.method + " " + st + " " + e.type + ms + " " + e.url;
}

function markEvents(tabId) {
  return {
    c: lastSeqOf(consoleBuf, tabId),
    d: lastSeqOf(dialogBuf, tabId)
  };
}

function attachEvents(tabId, mark, result) {
  if (!result || typeof result !== "object") return result;
  const dialogs = (dialogBuf.get(tabId) || []).filter((e) => e.seq > mark.d);
  if (dialogs.length) {
    result.dialogs = dialogs.slice(0, 5).map(formatDialog);
  }
  const errs = (consoleBuf.get(tabId) || []).filter(
    (e) => e.seq > mark.c && (e.level === "error" || e.level === "exception")
  );
  if (errs.length) {
    result.errors = errs.slice(0, 3).map((e) => e.text.slice(0, 160));
    if (errs.length > 3) result.errors_more = errs.length - 3;
  }
  return result;
}

// ---- Gezinme izleme ----
browser.webNavigation.onBeforeNavigate.addListener((d) => {
  if (d.frameId !== 0) return;
  navSeq.set(d.tabId, (navSeq.get(d.tabId) || 0) + 1);
  navPending.set(d.tabId, true);
  if (d.url) tabUrls.set(d.tabId, d.url);
  const nextOrig = originOf(d.url || "");
  if (nextOrig) {
    const prevOrig = tabOrigins.get(d.tabId);
    if (prevOrig && prevOrig !== nextOrig) {
      consoleBuf.delete(d.tabId);
      networkBuf.delete(d.tabId);
    }
    tabOrigins.set(d.tabId, nextOrig);
  }
});
browser.webNavigation.onCommitted.addListener((d) => {
  if (d.frameId !== 0) return;
  if (d.url) tabUrls.set(d.tabId, d.url);
  const nextOrig = originOf(d.url || "");
  if (nextOrig) {
    const prevOrig = tabOrigins.get(d.tabId);
    if (prevOrig && prevOrig !== nextOrig) {
      consoleBuf.delete(d.tabId);
    }
    tabOrigins.set(d.tabId, nextOrig);
  }
});
const navDone = (d) => {
  if (d.frameId === 0) navPending.set(d.tabId, false);
};
browser.webNavigation.onCompleted.addListener(navDone);
browser.webNavigation.onErrorOccurred.addListener(navDone);
// Parça (#) gezinmesinde onCompleted gelmeyebilir; beklemeyi takılı bırakma.
browser.webNavigation.onReferenceFragmentUpdated.addListener(navDone);
browser.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (changeInfo.url) tabUrls.set(tabId, changeInfo.url);
  if (changeInfo.status === "complete") navPending.set(tabId, false);
}, { properties: ["status", "url"] });
browser.tabs.onRemoved.addListener((tabId) => {
  navSeq.delete(tabId);
  navPending.delete(tabId);
  consoleBuf.delete(tabId);
  dialogBuf.delete(tabId);
  networkBuf.delete(tabId);
  tabOrigins.delete(tabId);
  tabUrls.delete(tabId);
  for (const [reqId, info] of reqMap) {
    if (info.tabId === tabId) reqMap.delete(reqId);
  }
});

// Yeni gezinme görüldüyse ve sekme yüklenmişse sekmeyi döner; süre dolarsa da döner.
// Gezinme hiç görülmezse (aynı belge) kısa bir gözlem payından sonra çıkar.
async function waitLoad(tabId, seq0, maxMs) {
  const t0 = Date.now();
  for (;;) {
    let tab;
    try {
      tab = await browser.tabs.get(tabId);
    } catch (e) {
      return null;
    }
    const seen = (navSeq.get(tabId) || 0) !== seq0;
    const idle = tab.status === "complete" && !navPending.get(tabId);
    const el = Date.now() - t0;
    if (idle && (seen || el >= 1500)) return tab;
    if (el >= maxMs) {
      // İndirme gibi olay üretmeyen gezinmeler bayrağı takılı bırakmasın.
      navPending.set(tabId, false);
      return tab;
    }
    await sleep(50);
  }
}

// ---- Yasak liste ----
function cleanList(v) {
  if (!Array.isArray(v)) return [];
  return v.filter((x) => typeof x === "string")
    .map((x) => x.trim().toLowerCase())
    .filter(Boolean);
}

function applyConfig(args) {
  const a = args && typeof args === "object" ? args : {};
  extraDeny = cleanList(a.deny);
  allowList = cleanList(a.allow);
}

function hostnameOf(url) {
  try {
    return new URL(url).hostname.toLowerCase();
  } catch (e) {
    return "";
  }
}

function deniedHost(url) {
  const host = hostnameOf(url);
  if (!host) return null;
  if (allowList.some((p) => host.includes(p))) return null;
  const pats = BUILTIN_DENY.concat(extraDeny).filter((p) => !allowList.includes(p));
  return pats.some((p) => host.includes(p)) ? host : null;
}

function assertAllowed(tab) {
  const h = deniedHost(tab.url || "");
  if (h) throw new Error("site yasak listesinde: " + h);
}

const FILE_URL_ERR =
  "file:// sayfaları köprüyle açılamaz ve okunamaz; dosyayı http üzerinden sunun " +
  "(ör. `python3 -m http.server` ile dizini sunup http://127.0.0.1:<port>/... açın)";

function assertScriptable(tab) {
  const url = tab.url || "";
  if (/^file:/i.test(url)) {
    throw new Error("bu sayfada içerik betiği çalışmaz: " + url + " — " + FILE_URL_ERR);
  }
  if (!/^(https?|ftp):/i.test(url)) {
    throw new Error("bu sayfada içerik betiği çalışmaz: " + url);
  }
}

// ---- Kanca olayları ve ağ kaydı ----
browser.runtime.onMessage.addListener((msg, sender) => {
  if (!msg || typeof msg !== "object" || typeof msg.hook !== "string") return undefined;
  if (msg.hook === "hello") {
    return Promise.resolve({ accept: dialogPolicy.accept, text: dialogPolicy.text });
  }
  if (msg.hook !== "event") return undefined;
  if (!sender || !sender.tab || typeof sender.tab.id !== "number" || sender.tab.id < 0) return undefined;

  const tabUrl = sender.tab.url || "";
  const frameUrl = typeof msg.url === "string" ? msg.url.slice(0, 200) : "";
  if (deniedHost(tabUrl) || (frameUrl && deniedHost(frameUrl))) return undefined;

  const tabId = sender.tab.id;
  const t = Date.now();
  const seq = ++eventSeq;
  const frame = typeof msg.frame === "string" ? msg.frame : "top";

  if (msg.kind === "dialog") {
    const type = msg.type === "alert" || msg.type === "confirm" || msg.type === "prompt" ? msg.type : "alert";
    const message = typeof msg.message === "string" ? msg.message.slice(0, 300) : "";
    const answer = msg.answer === undefined ? undefined : msg.answer;
    pushRing(dialogBuf, tabId, { seq, t, type, message, answer, frame, url: frameUrl }, MAX_DIALOGS);
  } else if (msg.kind === "console") {
    const level = (msg.level === "error" || msg.level === "warn" || msg.level === "exception" || msg.level === "resource")
      ? msg.level
      : "warn";
    const text = typeof msg.text === "string" ? msg.text.slice(0, 500) : "";
    pushRing(consoleBuf, tabId, { seq, t, level, text, frame, url: frameUrl }, MAX_CONSOLE);
  }
  return undefined;
});

function isDeniedRequest(details) {
  if (deniedHost(details.url || "")) return true;
  const tabUrl = tabUrls.get(details.tabId) || "";
  if (tabUrl && deniedHost(tabUrl)) return true;
  if (details.documentUrl && deniedHost(details.documentUrl)) return true;
  if (details.originUrl && deniedHost(details.originUrl)) return true;
  return false;
}

if (browser.webRequest) {
  browser.webRequest.onBeforeRequest.addListener((details) => {
    if (details.tabId < 0) return;
    if (details.type === "main_frame" && details.url) {
      tabUrls.set(details.tabId, details.url);
    }
    if (isDeniedRequest(details)) return;
    const t = Date.now();
    const entry = {
      seq: ++eventSeq,
      t,
      method: details.method || "GET",
      url: String(details.url || "").slice(0, 300),
      type: details.type || "other",
      status: null,
      error: null,
      ms: null,
      from_cache: false
    };
    if (reqMap.size >= MAX_REQ_MAP) {
      const oldest = reqMap.keys().next().value;
      if (oldest !== undefined) reqMap.delete(oldest);
    }
    reqMap.set(details.requestId, { tabId: details.tabId, entry, t0: t });
    pushRing(networkBuf, details.tabId, entry, MAX_NETWORK);
  }, { urls: ["<all_urls>"] });

  browser.webRequest.onCompleted.addListener((details) => {
    const info = reqMap.get(details.requestId);
    if (!info) return;
    reqMap.delete(details.requestId);
    info.entry.status = typeof details.statusCode === "number" ? details.statusCode : 0;
    info.entry.from_cache = Boolean(details.fromCache);
    info.entry.ms = Math.max(0, Date.now() - info.t0);
  }, { urls: ["<all_urls>"] });

  browser.webRequest.onErrorOccurred.addListener((details) => {
    const info = reqMap.get(details.requestId);
    if (!info) return;
    reqMap.delete(details.requestId);
    info.entry.error = details.error ? String(details.error) : "ERR";
    info.entry.from_cache = Boolean(details.fromCache);
    info.entry.ms = Math.max(0, Date.now() - info.t0);
  }, { urls: ["<all_urls>"] });
}

browser.tabs.query({}).then((tabs) => {
  for (const t of tabs) {
    if (t.url) {
      tabUrls.set(t.id, t.url);
      const orig = originOf(t.url);
      if (orig) tabOrigins.set(t.id, orig);
    }
  }
}).catch(() => {});

// ---- Sekme çözümleme ----
async function resolveTab(args) {
  if (args.tab !== undefined && args.tab !== null) {
    const id = Number(args.tab);
    if (!Number.isInteger(id)) throw new Error("geçersiz tab: " + args.tab);
    try {
      return await browser.tabs.get(id);
    } catch (e) {
      throw new Error("sekme bulunamadı: " + id);
    }
  }
  let list = await browser.tabs.query({ active: true, lastFocusedWindow: true, windowType: "normal" });
  if (!list.length) {
    const wins = await browser.windows.getAll({ windowTypes: ["normal"] });
    const w = wins.find((x) => x.focused) || wins[0];
    if (w) list = await browser.tabs.query({ active: true, windowId: w.id });
  }
  if (!list.length) throw new Error("etkin sekme bulunamadı");
  return list[0];
}

function tabInfo(t) {
  const url = t.url || "";
  // Yasak sayfalara varıldığında başlık sızmasın ve durum işaretlensin.
  if (deniedHost(url)) return { tab: t.id, url, title: "", denied: true };
  return { tab: t.id, url, title: t.title || "" };
}

// ---- İçerik betiği çağrısı ----
async function injectContent(tab) {
  // all_frames ile betik çapraz kaynaklı alt çerçevelere de girer; yalnız onlar köprüyü kurar.
  await browser.tabs.executeScript(tab.id, { file: "content.js", allFrames: true, runAt: "document_start" });
  // Eklenti sayfadan sonra kurulduysa kancalar da (tüm çerçevelere) sonradan eklenir.
  browser.tabs.executeScript(tab.id, { file: "hooks.js", allFrames: true, runAt: "document_start" }).catch(() => {});
}

// Dönüş: {lost:false,result,seq0} ya da gezinme yüzünden yanıt kaybolduysa {lost:true,seq0}.
async function pageCall(tab0, op, args, frameId = 0) {
  if (navPending.get(tab0.id)) {
    // Yüklenmekte olan belgeye mesaj göndermek yanıtsız kalabilir.
    await waitLoad(tab0.id, (navSeq.get(tab0.id) || 0) - 1, 3000);
  }
  let tab;
  try {
    tab = await browser.tabs.get(tab0.id);
  } catch (e) {
    throw new Error("sekme bulunamadı: " + tab0.id);
  }
  assertAllowed(tab);
  assertScriptable(tab);

  const seq0 = navSeq.get(tab.id) || 0;
  const navStarted = () => (navSeq.get(tab.id) || 0) !== seq0;
  let reply;
  try {
    // frameId verilmezse ileti tüm çerçevelere gider ve yanıtlar yarışır; 0 = üst çerçeve.
    reply = await browser.tabs.sendMessage(tab.id, { op, args }, { frameId });
  } catch (e1) {
    if (navStarted()) return { lost: true, seq0 };
    // Sayfa eklentiden önce açılmış olabilir: betiği enjekte edip bir kez dene.
    try {
      await injectContent(tab);
    } catch (e2) {
      // Gerçek enjeksiyon hatası gizlenmesin, teşhis için eklensin.
      throw new Error("bu sayfada içerik betiği çalışmaz: " + (tab.url || "") + " (" + errText(e2) + ")");
    }
    try {
      reply = await browser.tabs.sendMessage(tab.id, { op, args }, { frameId });
    } catch (e3) {
      if (navStarted()) return { lost: true, seq0 };
      throw new Error("içerik betiği yanıt vermedi: " + errText(e3));
    }
  }
  if (!reply || typeof reply !== "object") {
    if (navStarted()) return { lost: true, seq0 };
    throw new Error("içerik betiği boş yanıt döndürdü");
  }
  if (reply.ok === false) throw new Error(String(reply.error || "bilinmeyen içerik hatası"));
  return { lost: false, result: reply.result, seq0 };
}

// Arka plan sekmesinde içerik zamanlayıcıları >=1 sn kısıldığından sakinleşmeyi background sürdürür.
async function settleInBackground(tab, res, seq0) {
  const t0 = Date.now();
  const initVer = res.version;
  let lastVer = initVer;
  let lastChangeAt = t0;
  let lastUrl = res.url;
  let lastTitle = res.title;

  for (;;) {
    if ((navSeq.get(tab.id) || 0) !== seq0) return { navigated: true };
    const left = 1500 - (Date.now() - t0);
    if (left <= 0) break;
    await sleep(Math.min(50, left));
    if ((navSeq.get(tab.id) || 0) !== seq0) return { navigated: true };
    let v;
    try {
      v = await pageCall(tab, "ver", {});
    } catch (e) {
      if ((navSeq.get(tab.id) || 0) !== seq0) return { navigated: true };
      throw e;
    }
    if (v.lost || (navSeq.get(tab.id) || 0) !== seq0) return { navigated: true };
    if (v.result && typeof v.result === "object") {
      if (typeof v.result.version === "number" && v.result.version !== lastVer) {
        lastVer = v.result.version;
        lastChangeAt = Date.now();
      }
      if (typeof v.result.url === "string") lastUrl = v.result.url;
      if (typeof v.result.title === "string") lastTitle = v.result.title;
    }
    const now = Date.now();
    if (now - lastChangeAt >= 150 || now - t0 >= 1500) break;
  }

  delete res.settle;
  res.version = lastVer;
  res.changed = Boolean(res.changed) || lastVer !== initVer;
  res.url = lastUrl;
  res.title = lastTitle;
  return { navigated: false, result: res };
}

// Eylem gezinme başlattıysa yeni belge yüklenene dek bekleyip sonucu background üretir.
async function afterAction(tab, r) {
  let navSeen = r.lost || (navSeq.get(tab.id) || 0) !== r.seq0;
  if (!navSeen) {
    if (r.result && r.result.settle === "background") {
      const settled = await settleInBackground(tab, r.result, r.seq0);
      if (!settled.navigated) return settled.result;
      navSeen = true;
    } else {
      return r.result;
    }
  }
  const t = await waitLoad(tab.id, r.seq0, NAV_WAIT_MS);
  if (!t) throw new Error("sekme kapandı");
  let version = 0;
  try {
    const v = await pageCall(t, "version", {});
    if (!v.lost && typeof v.result === "number") version = v.result;
  } catch (e) {
    // Yeni belgede içerik betiği yoksa sayaç 0 kalır.
  }
  return { ok: true, url: t.url || "", title: t.title || "", version, navigated: true, changed: true };
}

// ---- Op'lar ----
async function listTabs() {
  const tabs = await browser.tabs.query({});
  return tabs.map((t) => ({
    id: t.id,
    window: t.windowId,
    active: !!t.active,
    title: t.title || "",
    url: t.url || "",
    status: t.status || ""
  }));
}

function normalizeUrl(u) {
  if (typeof u !== "string" || !u.trim()) throw new Error("url gerekli");
  const s = u.trim();
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(s) || /^(about|data|file|view-source|mailto|moz-extension):/i.test(s)) return s;
  return "https://" + s;
}

async function opNavigate(args) {
  const url = normalizeUrl(args.url);
  if (/^file:/i.test(url)) throw new Error(FILE_URL_ERR);
  // Hedef yasaksa gezinmeden reddet; mevcut sekme yasak olsa da uzaklaşmaya izin ver.
  assertAllowed({ url });
  if (args.new_tab) {
    const t = await browser.tabs.create({ url, active: true });
    const mark = { c: 0, d: 0 };
    // Yeni sekmenin kimliği benzersiz: sayaç 0'dan başlar.
    const done = await waitLoad(t.id, 0, LOAD_WAIT_MS);
    const res = tabInfo(done || t);
    return res.denied ? res : attachEvents(t.id, mark, res);
  }
  const tab = await resolveTab(args);
  const mark = markEvents(tab.id);
  const seq0 = navSeq.get(tab.id) || 0;
  await browser.tabs.update(tab.id, { url });
  const t = await waitLoad(tab.id, seq0, LOAD_WAIT_MS);
  if (!t) throw new Error("sekme kapandı");
  const res = tabInfo(t);
  return res.denied ? res : attachEvents(tab.id, mark, res);
}

async function opHistory(kind, args) {
  const tab = await resolveTab(args);
  const mark = markEvents(tab.id);
  // Yasak sekmeden çıkış/yenileme engellenmesin; varış kontrolü tabInfo'da yapılır.
  const seq0 = navSeq.get(tab.id) || 0;
  if (kind === "back") await browser.tabs.goBack(tab.id);
  else if (kind === "forward") await browser.tabs.goForward(tab.id);
  else await browser.tabs.reload(tab.id);
  const t = await waitLoad(tab.id, seq0, LOAD_WAIT_MS);
  if (!t) throw new Error("sekme kapandı");
  const res = tabInfo(t);
  return res.denied ? res : attachEvents(tab.id, mark, res);
}

function requireTabId(args) {
  const id = Number(args.tab);
  if (args.tab === undefined || args.tab === null || !Number.isInteger(id)) {
    throw new Error("tab gerekli");
  }
  return id;
}

async function opActivate(args) {
  const id = requireTabId(args);
  let t;
  try {
    t = await browser.tabs.get(id);
  } catch (e) {
    throw new Error("sekme bulunamadı: " + id);
  }
  // Yasak sekme de etkinleştirilebilsin; içerik erişimi diğer op'larda kısıtlı.
  await browser.tabs.update(id, { active: true });
  try {
    // Gerçek fare tıklaması için pencere de öne gelmeli.
    await browser.windows.update(t.windowId, { focused: true });
  } catch (e) {
    // Pencere odağı alınamazsa sekme yine de etkinleşmiştir.
  }
  return { ok: true };
}

async function opClose(args) {
  const id = requireTabId(args);
  let t;
  try {
    t = await browser.tabs.get(id);
  } catch (e) {
    throw new Error("sekme bulunamadı: " + id);
  }
  // Yasak sekme kapatılabilsin ki ajan takılı kalmasın.
  await browser.tabs.remove(id);
  return { ok: true };
}

async function opEval(args) {
  if (typeof args.code !== "string" || !args.code) throw new Error("code gerekli");
  const tab = await resolveTab(args);
  assertAllowed(tab);
  assertScriptable(tab);
  let res;
  try {
    res = await browser.tabs.executeScript(tab.id, { code: args.code });
  } catch (e) {
    throw new Error(errText(e));
  }
  const v = Array.isArray(res) ? res[0] : res;
  // JSON'a çevrilebilirlik: undefined -> null, döngüsel yapı hata verir.
  const value = v === undefined ? null : JSON.parse(JSON.stringify(v));
  return { value };
}

async function opConsole(args) {
  const tab = await resolveTab(args);
  assertAllowed(tab);
  const all = consoleBuf.get(tab.id) || [];
  const total = all.length;
  const since = args.since !== undefined && args.since !== null ? numOr(args.since, 0) : null;
  const lvl = typeof args.level === "string" && args.level ? args.level.toLowerCase() : "";
  const pat = typeof args.pattern === "string" && args.pattern ? args.pattern.toLowerCase() : "";
  const limit = Math.max(0, Math.floor(numOr(args.limit, 50)));

  const filtered = all.filter((e) => {
    if (since !== null && e.seq <= since) return false;
    if (lvl === "error") {
      if (e.level !== "error" && e.level !== "exception" && e.level !== "resource") return false;
    } else if (lvl && e.level !== lvl) {
      return false;
    }
    if (pat && !e.text.toLowerCase().includes(pat)) return false;
    return true;
  });

  const picked = limit > 0 ? filtered.slice(-limit) : [];
  const lines = picked.map((e) => "[" + e.level + "] " + e.text + " (" + e.frame + ")");
  const lastSeq = all.length ? all[all.length - 1].seq : 0;
  if (args.clear) consoleBuf.delete(tab.id);
  return { tab: tab.id, count: lines.length, total, last_seq: lastSeq, lines };
}

async function opNetwork(args) {
  const tab = await resolveTab(args);
  assertAllowed(tab);
  const all = networkBuf.get(tab.id) || [];
  const total = all.length;
  const since = args.since !== undefined && args.since !== null ? numOr(args.since, 0) : null;
  const pat = typeof args.pattern === "string" && args.pattern ? args.pattern.toLowerCase() : "";
  const failedOnly = Boolean(args.failed);
  const limit = Math.max(0, Math.floor(numOr(args.limit, 50)));

  const filtered = all.filter((e) => {
    if (since !== null && e.seq <= since) return false;
    if (failedOnly && !e.error && !(typeof e.status === "number" && e.status >= 400)) return false;
    if (pat && !e.url.toLowerCase().includes(pat)) return false;
    return true;
  });

  const picked = limit > 0 ? filtered.slice(-limit) : [];
  const lines = picked.map(formatNet);
  const lastSeq = all.length ? all[all.length - 1].seq : 0;
  if (args.clear) networkBuf.delete(tab.id);
  return { tab: tab.id, count: lines.length, total, last_seq: lastSeq, lines };
}

async function opDialogs(args) {
  const tab = await resolveTab(args);
  assertAllowed(tab);

  let changed = false;
  if (typeof args.accept === "boolean") {
    dialogPolicy = { accept: args.accept, text: dialogPolicy.text };
    changed = true;
  }
  if (args.text !== undefined) {
    const txt = args.text === null ? null : String(args.text);
    dialogPolicy = { accept: dialogPolicy.accept, text: txt };
    changed = true;
  }
  if (changed) {
    try {
      const p = browser.tabs.sendMessage(tab.id, {
        hook: "policy",
        accept: dialogPolicy.accept,
        text: dialogPolicy.text
      });
      if (p && typeof p.catch === "function") p.catch(() => {});
    } catch (e) {}
  }

  const all = dialogBuf.get(tab.id) || [];
  const since = args.since !== undefined && args.since !== null ? numOr(args.since, 0) : null;
  const limit = Math.max(0, Math.floor(numOr(args.limit, 20)));
  const filtered = since !== null ? all.filter((e) => e.seq > since) : all;
  const picked = limit > 0 ? filtered.slice(-limit) : [];
  const lines = picked.map(formatDialog);
  const lastSeq = all.length ? all[all.length - 1].seq : 0;
  if (args.clear) dialogBuf.delete(tab.id);
  return {
    tab: tab.id,
    policy: { accept: dialogPolicy.accept, text: dialogPolicy.text },
    count: lines.length,
    last_seq: lastSeq,
    lines
  };
}

async function opWait(tab, args) {
  const total = Math.max(0, numOr(args.timeout_ms, 5000));
  const t0 = Date.now();
  let a = Object.assign({}, args, { timeout_ms: total });
  for (let i = 0; i < 4; i++) {
    const r = await pageCall(tab, "wait", a);
    if (!r.lost) {
      const res = r.result;
      if (i > 0 && res && typeof res === "object") res.elapsed_ms = Date.now() - t0;
      return res;
    }
    // Bekleme sırasında sayfa gezindi: yeni belgede kalan süreyle sürdür.
    const left0 = total - (Date.now() - t0);
    await waitLoad(tab.id, r.seq0, Math.min(NAV_WAIT_MS, Math.max(300, left0)));
    a = Object.assign({}, args, { timeout_ms: Math.max(0, total - (Date.now() - t0)) });
  }
  let url = "";
  try {
    url = (await browser.tabs.get(tab.id)).url || "";
  } catch (e) {
    url = "";
  }
  return { matched: false, elapsed_ms: Date.now() - t0, url };
}

// Çerçeve içi ref'ler ajana "e5f12" biçiminde sunulur; üst çerçeve ref'leri çıplak kalır.
function splitFrameRef(ref) {
  const m = typeof ref === "string" ? /^(e\d+)f(\d+)$/.exec(ref) : null;
  if (m) return { ref: m[1], frameId: Number(m[2]) };
  return { ref, frameId: 0 };
}

// Yalnız satır başındaki [eN] kalıbı çerçeve kimliğiyle etiketlenir.
function tagFrameLines(lines, frameId) {
  if (!Array.isArray(lines)) return [];
  return lines.map((l) => String(l).replace(/^\[(e\d+)\]/, "[$1f" + frameId + "]"));
}

// Alt çerçeve snapshot'ları üst sonuçla birleştirilir; aynı kaynaklı çerçevede betik susar, hata normaldir.
async function collectFrameSnapshots(tab, a, result) {
  if (!result || !Array.isArray(result.lines)) return;
  let frames = [];
  try {
    frames = await browser.webNavigation.getAllFrames({ tabId: tab.id });
  } catch (e) {
    frames = [];
  }
  if (!Array.isArray(frames)) return;
  const cands = frames.filter((f) => f && f.frameId !== 0 && typeof f.url === "string" &&
    /^https?:/i.test(f.url) && !deniedHost(f.url)).slice(0, 8);
  const total = numOr(a.max, 150);
  for (const f of cands) {
    const kalan = total - result.lines.length;
    if (kalan <= 0) return;
    let reply;
    try {
      reply = await Promise.race([
        browser.tabs.sendMessage(tab.id,
          { op: "snapshot", args: Object.assign({}, a, { max: Math.min(40, kalan) }) },
          { frameId: f.frameId }),
        new Promise((_, rej) => setTimeout(() => rej(new Error("çerçeve snapshot zaman aşımı")), 800))
      ]);
    } catch (e) {
      continue;
    }
    if (!reply || typeof reply !== "object" || reply.ok === false) continue;
    const fr = reply.result;
    if (!fr || !Array.isArray(fr.lines) || !fr.lines.length) continue;
    result.lines.push("frame f" + f.frameId + " " + JSON.stringify(String(fr.url || f.url || "").slice(0, 80)));
    for (const line of tagFrameLines(fr.lines, f.frameId)) result.lines.push(line);
    result.count = result.lines.length;
    if (fr.truncated) result.truncated = true;
    if (fr.focused && !result.focused) result.focused = fr.focused + "f" + f.frameId;
  }
}

async function broadcastPolicy(tabId, policy) {
  try {
    await browser.tabs.sendMessage(tabId, { hook: "policy", accept: policy.accept, text: policy.text });
  } catch (e) {
    // Kancasız sayfada alıcı yoktur; yok sayılır.
  }
}

// Eyleme özel diyalog yanıtı (`dialog: "accept"|"dismiss"`, `dialog_text`): yalnız bu eylem boyunca geçerlidir.
async function opPage(op, args) {
  const tab = await resolveTab(args);
  assertAllowed(tab);
  const a = Object.assign({}, args);
  delete a.tab;
  const hasDialog = a.dialog !== undefined || a.dialog_text !== undefined;
  const temp = {
    accept: a.dialog_text !== undefined ? a.dialog !== "dismiss" : a.dialog === "accept",
    text: a.dialog_text !== undefined && a.dialog_text !== null ? String(a.dialog_text) : null
  };
  delete a.dialog;
  delete a.dialog_text;
  if (!hasDialog || !ACTION_OPS.has(op)) return opPageInner(tab, op, a);
  await broadcastPolicy(tab.id, temp);
  try {
    return await opPageInner(tab, op, a);
  } finally {
    await broadcastPolicy(tab.id, dialogPolicy);
  }
}

async function opPageInner(tab, op, a) {
  if (op === "wait") return opWait(tab, a);
  const isAction = ACTION_OPS.has(op);
  const mark = isAction ? markEvents(tab.id) : null;
  const fr = splitFrameRef(a.ref);
  if (fr.frameId !== 0) {
    if (op === "rect") throw new Error("çerçeve içi eleman için gerçek işaretçi (real) desteklenmez: " + a.ref);
    a.ref = fr.ref;
    const rf = await pageCall(tab, op, a, fr.frameId);
    // Çerçeve gezindiyse ref geçersizdir; ajan yeni snapshot almalı.
    if (rf.lost) return { ok: true, changed: true, frame_navigated: true };
    const res = rf.result;
    delete res.settle;
    // Çerçevenin kendi adresi ajanı yanıltmasın diye sekmenin url'si ve başlığı döner.
    const t = await browser.tabs.get(tab.id);
    res.url = t.url || "";
    res.title = t.title || "";
    res.frame = fr.frameId;
    return isAction ? attachEvents(tab.id, mark, res) : res;
  }
  const sf = splitFrameRef(a.scope);
  if (op === "snapshot" && sf.frameId !== 0) {
    const scopeRef = a.scope;
    a.scope = sf.ref;
    const rs = await pageCall(tab, op, a, sf.frameId);
    if (rs.lost) throw new Error("çerçeve gezindi, snapshot tamamlanamadı: " + scopeRef);
    const res = rs.result;
    res.lines = tagFrameLines(res.lines, sf.frameId);
    return Object.assign({ tab: tab.id }, res);
  }
  let r = await pageCall(tab, op, a);
  if (r.lost && !isAction) {
    await waitLoad(tab.id, r.seq0, NAV_WAIT_MS);
    r = await pageCall(tab, op, a);
    if (r.lost) throw new Error("sayfa gezindi, işlem tamamlanamadı: " + op);
  }
  if (isAction) return attachEvents(tab.id, mark, await afterAction(tab, r));
  if (op === "snapshot") {
    // Kapsam verilmemişse alt çerçeveler de taranır; kapsam varsa yalnız o kök okunur.
    if (!a.scope) await collectFrameSnapshots(tab, a, r.result);
    return Object.assign({ tab: tab.id }, r.result);
  }
  return r.result;
}

async function dispatch(op, args) {
  switch (op) {
    case "tabs":
      return listTabs();
    case "ping": {
      const all = await browser.tabs.query({});
      return { version: browser.runtime.getManifest().version, tabs: all.length };
    }
    case "navigate":
      return opNavigate(args);
    case "back":
    case "forward":
    case "reload":
      return opHistory(op, args);
    case "activate":
      return opActivate(args);
    case "close":
      return opClose(args);
    case "eval":
      return opEval(args);
    case "console":
      return opConsole(args);
    case "network":
      return opNetwork(args);
    case "dialogs":
      return opDialogs(args);
    default:
      if (PAGE_OPS.has(op)) return opPage(op, args);
      throw new Error("bilinmeyen op: " + op);
  }
}

function timeoutFor(op, args) {
  if (op === "wait") return Math.max(0, numOr(args.timeout_ms, 5000)) + 2000;
  if (op === "navigate" || op === "back" || op === "forward" || op === "reload") return NAVIGATE_TIMEOUT_MS;
  return DEFAULT_TIMEOUT_MS;
}

// Her isteğe tam bir yanıt: başarı, hata ya da zaman aşımı (hangisi önce gelirse).
function handleRequest(p, msg) {
  const id = msg.id;
  const op = msg.op;
  const args = msg.args && typeof msg.args === "object" ? msg.args : {};
  let done = false;
  let timer = null;
  const finish = (payload) => {
    if (done) return;
    done = true;
    clearTimeout(timer);
    post(p, Object.assign({ id }, payload));
  };
  if (typeof op !== "string" || !op) {
    finish({ ok: false, error: "op gerekli" });
    return;
  }
  const ms = timeoutFor(op, args);
  timer = setTimeout(() => finish({ ok: false, error: "zaman aşımı: " + op + " (" + ms + " ms)" }), ms);
  (async () => {
    try {
      const result = await dispatch(op, args);
      finish({ ok: true, result: result === undefined ? null : result });
    } catch (e) {
      finish({ ok: false, error: errText(e) });
    }
  })();
}

function onHostMessage(p, msg) {
  if (!msg || typeof msg !== "object") return;
  if (typeof msg.id !== "number" || !Number.isFinite(msg.id)) {
    console.warn("pc-eye: id'siz ileti yok sayıldı");
    return;
  }
  // config yalnız ana makineden (id===0) uygulanır; diğer istekler reddedilir.
  if (msg.op === "config") {
    if (msg.id === 0) applyConfig(msg.args);
    else post(p, { id: msg.id, ok: false, error: "config yalnız ana makineden gelir" });
    return;
  }
  handleRequest(p, msg);
}

// ---- Bağlantı ----
function scheduleReconnect() {
  if (reconnectTimer) return;
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, RECONNECT_MS);
}

function connect() {
  let p;
  try {
    p = browser.runtime.connectNative(NATIVE_APP);
  } catch (e) {
    console.warn("pc-eye: connectNative başarısız:", errText(e));
    scheduleReconnect();
    return;
  }
  port = p;
  p.onMessage.addListener((m) => onHostMessage(p, m));
  p.onDisconnect.addListener(() => {
    const err = p.error ? p.error.message : "";
    console.warn("pc-eye: ana makine bağlantısı koptu", err);
    // Eski bağlantının geç kopması yeni bağlantıyı etkilemesin.
    if (port === p) {
      port = null;
      scheduleReconnect();
    }
  });
  post(p, { event: "hello", version: browser.runtime.getManifest().version, ua: navigator.userAgent });
}

connect();
