"use strict";

// ---- Sabitler ----
const NATIVE_APP = "pc_eye_bridge";
const RECONNECT_MS = 2000;
const DEFAULT_TIMEOUT_MS = 10000;
const NAVIGATE_TIMEOUT_MS = 20000;
const NAV_WAIT_MS = 5000;
const LOAD_WAIT_MS = 18000;

const BUILTIN_DENY = [
  "bank", "banka", "paypal", "wise.com", "binance", "coinbase", "1password",
  "bitwarden", "lastpass", "keepass", "e-devlet", "turkiye.gov.tr",
  "accounts.google.com", "login.microsoftonline.com"
];

const PAGE_OPS = new Set([
  "snapshot", "click", "type", "select", "check", "hover", "focus", "key",
  "scroll", "text", "rect", "wait"
]);
// Gezinme başlatabilen eylemler: yanıt kaybolursa background sonucu üretir.
const ACTION_OPS = new Set(["click", "type", "select", "check", "hover", "focus", "key"]);

// ---- Durum ----
let port = null;
let reconnectTimer = null;
let extraDeny = [];
let allowList = [];
const navSeq = new Map();      // sekme -> ana çerçeve gezinme sayacı
const navPending = new Map();  // sekme -> gezinme sürüyor mu

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

// ---- Gezinme izleme ----
browser.webNavigation.onBeforeNavigate.addListener((d) => {
  if (d.frameId !== 0) return;
  navSeq.set(d.tabId, (navSeq.get(d.tabId) || 0) + 1);
  navPending.set(d.tabId, true);
});
const navDone = (d) => {
  if (d.frameId === 0) navPending.set(d.tabId, false);
};
browser.webNavigation.onCompleted.addListener(navDone);
browser.webNavigation.onErrorOccurred.addListener(navDone);
// Parça (#) gezinmesinde onCompleted gelmeyebilir; beklemeyi takılı bırakma.
browser.webNavigation.onReferenceFragmentUpdated.addListener(navDone);
browser.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (changeInfo.status === "complete") navPending.set(tabId, false);
}, { properties: ["status"] });
browser.tabs.onRemoved.addListener((tabId) => {
  navSeq.delete(tabId);
  navPending.delete(tabId);
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

function assertScriptable(tab) {
  if (!/^(https?|file|ftp):/i.test(tab.url || "")) {
    throw new Error("bu sayfada içerik betiği çalışmaz: " + (tab.url || ""));
  }
}

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
  await browser.tabs.executeScript(tab.id, { file: "content.js", runAt: "document_start" });
}

// Dönüş: {lost:false,result,seq0} ya da gezinme yüzünden yanıt kaybolduysa {lost:true,seq0}.
async function pageCall(tab0, op, args) {
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
    reply = await browser.tabs.sendMessage(tab.id, { op, args });
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
      reply = await browser.tabs.sendMessage(tab.id, { op, args });
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

// Eylem gezinme başlattıysa yeni belge yüklenene dek bekleyip sonucu background üretir.
async function afterAction(tab, r) {
  const navSeen = r.lost || (navSeq.get(tab.id) || 0) !== r.seq0;
  if (!navSeen) return r.result;
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
  // Hedef yasaksa gezinmeden reddet; mevcut sekme yasak olsa da uzaklaşmaya izin ver.
  assertAllowed({ url });
  if (args.new_tab) {
    const t = await browser.tabs.create({ url, active: true });
    // Yeni sekmenin kimliği benzersiz: sayaç 0'dan başlar.
    const done = await waitLoad(t.id, 0, LOAD_WAIT_MS);
    return tabInfo(done || t);
  }
  const tab = await resolveTab(args);
  const seq0 = navSeq.get(tab.id) || 0;
  await browser.tabs.update(tab.id, { url });
  const t = await waitLoad(tab.id, seq0, LOAD_WAIT_MS);
  if (!t) throw new Error("sekme kapandı");
  return tabInfo(t);
}

async function opHistory(kind, args) {
  const tab = await resolveTab(args);
  // Yasak sekmeden çıkış/yenileme engellenmesin; varış kontrolü tabInfo'da yapılır.
  const seq0 = navSeq.get(tab.id) || 0;
  if (kind === "back") await browser.tabs.goBack(tab.id);
  else if (kind === "forward") await browser.tabs.goForward(tab.id);
  else await browser.tabs.reload(tab.id);
  const t = await waitLoad(tab.id, seq0, LOAD_WAIT_MS);
  if (!t) throw new Error("sekme kapandı");
  return tabInfo(t);
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

async function opPage(op, args) {
  const tab = await resolveTab(args);
  assertAllowed(tab);
  const a = Object.assign({}, args);
  delete a.tab;
  if (op === "wait") return opWait(tab, a);
  const isAction = ACTION_OPS.has(op);
  let r = await pageCall(tab, op, a);
  if (r.lost && !isAction) {
    await waitLoad(tab.id, r.seq0, NAV_WAIT_MS);
    r = await pageCall(tab, op, a);
    if (r.lost) throw new Error("sayfa gezindi, işlem tamamlanamadı: " + op);
  }
  if (isAction) return afterAction(tab, r);
  if (op === "snapshot") return Object.assign({ tab: tab.id }, r.result);
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
