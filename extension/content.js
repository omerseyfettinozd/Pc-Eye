(() => {
  "use strict";
  if (window !== window.top || window.__pcEyeBridge) return;
  window.__pcEyeBridge = true;

  // ---- Durum ve DOM gözlemcisi ----
  let version = 0;
  let lastMutAt = Date.now();
  let nextRefId = 1;
  const elToRef = new WeakMap();
  const refToEl = new Map();
  const mutWaiters = new Set();
  const observedRoots = new WeakSet();

  const mutObs = new MutationObserver(() => {
    version++;
    lastMutAt = Date.now();
    for (const fn of mutWaiters) fn();
  });

  function observeRoot(root) {
    if (!root || observedRoots.has(root)) return;
    observedRoots.add(root);
    try {
      mutObs.observe(root, { childList: true, subtree: true, attributes: true, characterData: true });
    } catch (_) {}
  }
  observeRoot(document);

  // ---- Genel yardımcılar ----
  const SKIP_TAGS = new Set(["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "HEAD", "META", "LINK", "TITLE"]);
  const INTERACTIVE_ROLES = new Set([
    "button", "link", "textbox", "searchbox", "combobox", "listbox", "option",
    "checkbox", "radio", "switch", "tab", "menuitem", "menuitemcheckbox",
    "menuitemradio", "slider", "spinbutton", "treeitem"
  ]);
  const BLOCK_TEXT_TAGS = new Set(["P", "LI", "BLOCKQUOTE", "PRE", "FIGCAPTION", "DT", "DD"]);
  const LIVE_ROLES = new Set(["alert", "status", "dialog", "alertdialog"]);

  const errText = (e) => (e && typeof e === "object" && e.message ? String(e.message) : String(e));
  const numOr = (v, def) => (v === undefined || v === null || v === "" || !Number.isFinite(Number(v)) ? def : Number(v));
  const norm = (s) => (s ? String(s).replace(/\s+/g, " ").trim() : "");
  const clip = (s, max = 80) => {
    const t = norm(s);
    return t.length > max ? t.slice(0, max - 1) + "…" : t;
  };
  const qStr = (s, max = 80) => '"' + clip(s, max).replace(/"/g, '\\"') + '"';

  function isConnectedDeep(el) {
    if (!el || !el.isConnected) return false;
    let doc = el.ownerDocument;
    while (doc && doc !== document) {
      const win = doc.defaultView;
      if (!win || !win.frameElement || !win.frameElement.isConnected) return false;
      doc = win.frameElement.ownerDocument;
    }
    return doc === document;
  }

  function getRef(el) {
    let r = elToRef.get(el);
    if (!r) {
      r = "e" + nextRefId++;
      elToRef.set(el, r);
      refToEl.set(r, new WeakRef(el));
    }
    return r;
  }

  function resolveRef(ref) {
    if (typeof ref !== "string" || !ref) throw new Error("geçersiz ref: " + ref);
    const wr = refToEl.get(ref);
    const el = wr ? wr.deref() : null;
    if (!isConnectedDeep(el)) {
      if (wr && !el) refToEl.delete(ref);
      throw new Error("ref bulunamadı: " + ref + " (yeni snapshot alın)");
    }
    return el;
  }

  function deepActiveElement() {
    let cur = document.activeElement;
    while (cur) {
      if (cur.shadowRoot && cur.shadowRoot.activeElement) {
        cur = cur.shadowRoot.activeElement;
        continue;
      }
      if (cur.tagName === "IFRAME" || cur.tagName === "FRAME") {
        try {
          const inner = cur.contentDocument && cur.contentDocument.activeElement;
          if (inner) {
            cur = inner;
            continue;
          }
        } catch (_) {}
      }
      break;
    }
    return cur && cur !== document.body && cur !== document.documentElement ? cur : null;
  }

  // ---- Geometri ve görünürlük ----
  function topViewportRect(el) {
    const r = el.getBoundingClientRect();
    let left = r.left;
    let top = r.top;
    let doc = el.ownerDocument;
    while (doc && doc !== document) {
      const fe = doc.defaultView && doc.defaultView.frameElement;
      if (!fe) break;
      const fr = fe.getBoundingClientRect();
      left += fr.left + (fe.clientLeft || 0);
      top += fr.top + (fe.clientTop || 0);
      doc = fe.ownerDocument;
    }
    return { left, top, right: left + r.width, bottom: top + r.height, width: r.width, height: r.height };
  }

  function isOffscreen(tr) {
    return tr.bottom <= 0 || tr.top >= window.innerHeight || tr.right <= 0 || tr.left >= window.innerWidth;
  }

  function scrollInfo() {
    const root = document.documentElement || document.body;
    const sh = root ? root.scrollHeight : 0;
    return {
      y: Math.round(window.scrollY || (root ? root.scrollTop : 0) || 0),
      max: Math.max(0, Math.round(sh - window.innerHeight))
    };
  }

  // ---- Güvenlik ve metin çıkarımı ----
  function isSecretField(el) {
    if (!el || el.nodeType !== 1) return false;
    if (el.tagName === "INPUT" && (el.type || "").toLowerCase() === "password") return true;
    const ac = (el.getAttribute("autocomplete") || "").toLowerCase().trim();
    if (!ac) return false;
    return ac.split(/\s+/).some((t) =>
      t.startsWith("cc-") || t === "current-password" || t === "new-password" || t === "one-time-code"
    );
  }

  function safeInnerText(el) {
    if (!el || isSecretField(el)) return "";
    if (el.tagName === "INPUT" || el.tagName === "TEXTAREA") return el.value || "";
    const secrets = [];
    if (el.querySelectorAll) {
      for (const s of el.querySelectorAll("textarea[autocomplete], [contenteditable][autocomplete]")) {
        if (isSecretField(s) && s.innerText) secrets.push(s.innerText);
      }
    }
    let txt = el.innerText || "";
    for (const s of secrets) txt = txt.split(s).join("");
    return txt;
  }

  function labelNodeText(label) {
    let out = "";
    const w = label.ownerDocument.createTreeWalker(label, NodeFilter.SHOW_TEXT | NodeFilter.SHOW_ELEMENT, {
      acceptNode(n) {
        if (n.nodeType === 1) {
          const t = n.tagName;
          if (t === "INPUT" || t === "SELECT" || t === "TEXTAREA" || t === "SCRIPT" || t === "STYLE") {
            return NodeFilter.FILTER_REJECT;
          }
          return NodeFilter.FILTER_SKIP;
        }
        return NodeFilter.FILTER_ACCEPT;
      }
    });
    let cur;
    while ((cur = w.nextNode())) out += " " + cur.nodeValue;
    return norm(out);
  }

  function accName(el, role) {
    const doc = el.ownerDocument || document;
    const lblBy = el.getAttribute("aria-labelledby");
    if (lblBy) {
      const parts = lblBy.trim().split(/\s+/).map((id) => {
        const ref = doc.getElementById(id);
        return ref ? labelNodeText(ref) : "";
      }).filter(Boolean);
      if (parts.length) return clip(parts.join(" "));
    }
    const ariaLbl = norm(el.getAttribute("aria-label"));
    if (ariaLbl) return clip(ariaLbl);

    let lblTxt = "";
    if (el.labels && el.labels.length) {
      lblTxt = Array.from(el.labels).map(labelNodeText).filter(Boolean).join(" ");
    } else if (el.closest) {
      const wrap = el.closest("label");
      if (wrap) lblTxt = labelNodeText(wrap);
    }
    if (lblTxt) return clip(lblTxt);

    const ph = norm(el.getAttribute("placeholder"));
    if (ph) return clip(ph);
    const title = norm(el.getAttribute("title"));
    if (title) return clip(title);
    const alt = norm(el.getAttribute("alt"));
    if (alt) return clip(alt);

    let textName = "";
    if (el.tagName === "INPUT") {
      const tp = (el.type || "").toLowerCase();
      if (tp === "button" || tp === "submit" || tp === "reset") textName = clip(el.value || tp);
    } else if (el.tagName !== "SELECT" && el.tagName !== "TEXTAREA" && !el.isContentEditable) {
      const txt = norm(el.innerText || el.textContent);
      if (txt) textName = clip(txt);
    }
    if (textName) return textName;
    if (INTERACTIVE_ROLES.has(role)) return fallbackAccName(el, doc);
    const img = el.querySelector && el.querySelector("img[alt]");
    return img ? clip(img.getAttribute("alt")) : "";
  }

  // Adı boş kalan etkileşimli elemanlar için son çare ad türetme.
  function fallbackAccName(el, doc) {
    const descBy = el.getAttribute("aria-describedby");
    if (descBy) {
      const parts = descBy.trim().split(/\s+/).map((id) => {
        const ref = doc.getElementById(id);
        return ref ? labelNodeText(ref) : "";
      }).filter(Boolean);
      if (parts.length) return clip(parts.join(" "), 60);
    }
    const svgTitle = el.querySelector && el.querySelector("svg title");
    if (svgTitle) {
      const t = norm(svgTitle.textContent);
      if (t) return clip(t, 60);
    }
    if (el.querySelectorAll) {
      for (const img of el.querySelectorAll("img[alt]")) {
        const a = norm(img.getAttribute("alt"));
        if (a) return clip(a, 60);
      }
    }
    const tip = norm(el.getAttribute("data-tooltip")) || norm(el.getAttribute("data-title"));
    if (tip) return clip(tip, 60);
    const nm = norm(el.getAttribute("name"));
    if (nm) return clip(nm, 60);
    const id = norm(el.getAttribute("id"));
    const digits = id ? (id.match(/\d/g) || []).length : 0;
    if (id && id.length < 30 && /[a-z]/i.test(id) && !/\d{4,}/.test(id) && digits <= id.length / 2) {
      return clip(id, 60);
    }
    return "";
  }

  // ---- Rol belirleme ve satır biçimi ----
  function resolveRole(el) {
    const explicit = (el.getAttribute("role") || "").toLowerCase().trim().split(/\s+/)[0];
    if (explicit) return explicit;
    const tag = el.tagName;
    if ((tag === "A" || tag === "AREA") && el.hasAttribute("href")) return "link";
    if (tag === "BUTTON" || tag === "SUMMARY") return "button";
    if (tag === "SELECT") return "combobox";
    if (tag === "TEXTAREA") return "textbox";
    if (tag === "INPUT") {
      const tp = (el.type || "text").toLowerCase();
      if (tp === "hidden") return "";
      if (tp === "button" || tp === "submit" || tp === "reset" || tp === "image" || tp === "file") return "button";
      if (tp === "checkbox") return "checkbox";
      if (tp === "radio") return "radio";
      if (tp === "range") return "slider";
      if (tp === "search") return "searchbox";
      if (tp === "number") return "spinbutton";
      return "textbox";
    }
    if (el.isContentEditable && (!el.parentElement || !el.parentElement.isContentEditable)) return "textbox";
    if (/^H[1-6]$/.test(tag)) return "heading";
    if (tag === "TR") return "row";
    if (el.hasAttribute("onclick")) return "button";
    return "";
  }

  function statusFlags(el, role, tr) {
    const flags = [];
    if (el.disabled || el.getAttribute("aria-disabled") === "true") flags.push("[disabled]");
    if (role === "checkbox" || role === "radio" || role === "switch" ||
        role === "menuitemcheckbox" || role === "menuitemradio") {
      if (el.checked || el.getAttribute("aria-checked") === "true") flags.push("[checked]");
    }
    if (el.getAttribute("aria-expanded") === "true" ||
        (el.tagName === "SUMMARY" && el.parentElement && el.parentElement.open)) {
      flags.push("[expanded]");
    }
    if (el.required || el.getAttribute("aria-required") === "true") flags.push("[required]");
    if (el.getAttribute("aria-invalid") === "true" ||
        (el.validity && !el.validity.valid && !el.validity.valueMissing)) {
      flags.push("[invalid]");
    }
    if (el.readOnly || el.getAttribute("aria-readonly") === "true") flags.push("[readonly]");
    if (isSecretField(el)) flags.push("[secret]");
    if (isOffscreen(tr)) flags.push("[offscreen]");
    return flags;
  }

  function rowText(el) {
    const cells = [];
    for (const ch of el.children) {
      const r = (ch.getAttribute("role") || "").toLowerCase();
      if (ch.tagName === "TD" || ch.tagName === "TH" || r === "cell" || r === "columnheader" || r === "rowheader") {
        const t = clip(safeInnerText(ch), 60);
        if (t) cells.push(t);
      }
    }
    return cells.join(" | ");
  }

  function formatInteractive(el, role, tr) {
    const ref = getRef(el);
    const name = accName(el, role);
    const secret = isSecretField(el);
    let val = "";
    let extra = "";
    if (!secret) {
      if (el.tagName === "SELECT") {
        const sel = Array.from(el.selectedOptions || []).map((o) => clip(o.text, 40)).filter(Boolean);
        val = sel.join(", ");
        const opts = Array.from(el.options || []).slice(0, 12).map((o) => clip(o.text, 30)).filter(Boolean);
        if (opts.length) extra += " {" + opts.join("|") + "}";
      } else if (role === "textbox" || role === "searchbox" || role === "spinbutton" || role === "slider") {
        val = el.isContentEditable ? clip(el.innerText, 80) : clip(el.value, 80);
      }
    }
    let href = "";
    if (role === "link") {
      href = clip(el.getAttribute("href") || "", 80);
      if (href) extra += " -> " + href;
    }
    const flags = statusFlags(el, role, tr);
    let line = `[${ref}] ${role}`;
    if (name) line += " " + qStr(name);
    if (val) line += " = " + qStr(val);
    line += extra;
    if (flags.length) line += " " + flags.join(" ");
    return { line, role, name, href, search: `${role} ${name} ${secret ? "" : val} ${href}`.toLowerCase() };
  }

  // ---- Snapshot toplama ----
  function opSnapshot(args) {
    const max = Math.max(1, numOr(args.max, 150));
    const query = typeof args.query === "string" && args.query.trim() ? args.query.trim().toLowerCase() : "";
    const viewportOnly = Boolean(args.viewport);
    const all = Boolean(args.all);
    const lines = [];
    let truncated = false;
    let prev = null;

    const pushLine = (entry, tr) => {
      if (viewportOnly && isOffscreen(tr)) return true;
      if (query && !entry.search.includes(query)) return true;
      // Ardışık aynı rol+adlı satırlarda hedefsiz veya aynı hedefli yinelemeyi ele.
      if (prev && entry.role && entry.name && prev.role === entry.role && prev.name === entry.name) {
        if (!prev.href && entry.href) {
          lines[lines.length - 1] = entry.line;
          prev = entry;
          return true;
        }
        if (!entry.href || prev.href === entry.href) return true;
      }
      if (lines.length >= max) {
        truncated = true;
        return false;
      }
      lines.push(entry.line);
      prev = entry;
      return true;
    };

    function walk(node, skipText) {
      if (truncated || !node || node.nodeType !== 1) return;
      if (SKIP_TAGS.has(node.tagName)) return;
      if (node.getAttribute("aria-hidden") === "true") return;

      const win = node.ownerDocument.defaultView || window;
      const cs = win.getComputedStyle(node);
      if (!cs || cs.display === "none") return;

      const selfVisible = cs.visibility !== "hidden" && cs.visibility !== "collapse";
      const tr = topViewportRect(node);
      const hasBox = tr.width > 0 && tr.height > 0;
      const role = resolveRole(node);
      let nextSkipText = skipText;

      if (selfVisible && hasBox && role) {
        if (INTERACTIVE_ROLES.has(role)) {
          if (!pushLine(formatInteractive(node, role, tr), tr)) return;
          nextSkipText = true;
        } else if (role === "heading") {
          const m = /^H([1-6])$/.exec(node.tagName);
          const lvl = node.getAttribute("aria-level") || (m ? m[1] : "2");
          const t = accName(node, role);
          if (t && !pushLine({ line: `heading(${lvl}) ${qStr(t, 120)}`, search: `heading ${t}`.toLowerCase() }, tr)) return;
          nextSkipText = true;
        } else if (role === "row") {
          const rt = rowText(node);
          if (rt && !pushLine({ line: `row ${qStr(rt, 160)}`, search: `row ${rt}`.toLowerCase() }, tr)) return;
          nextSkipText = true;
        } else if (LIVE_ROLES.has(role)) {
          const t = role === "dialog" || role === "alertdialog"
            ? (accName(node, role) || clip(safeInnerText(node), 120))
            : clip(safeInnerText(node), 120);
          if (t && !pushLine({ line: `${role} ${qStr(t, 120)}`, search: `${role} ${t}`.toLowerCase() }, tr)) return;
        }
      }

      if (all && selfVisible && hasBox && !nextSkipText && BLOCK_TEXT_TAGS.has(node.tagName)) {
        const hasSubBlock = node.querySelector("p,li,blockquote,pre,h1,h2,h3,h4,h5,h6,table,div");
        if (!hasSubBlock) {
          const t = clip(safeInnerText(node), 120);
          if (t && !pushLine({ line: `text ${qStr(t, 120)}`, search: `text ${t}`.toLowerCase() }, tr)) return;
          nextSkipText = true;
        }
      }

      if (node.tagName === "SVG" && !INTERACTIVE_ROLES.has(role)) return;

      if (node.shadowRoot) {
        observeRoot(node.shadowRoot);
        for (const ch of node.shadowRoot.children) {
          walk(ch, nextSkipText);
          if (truncated) return;
        }
      }
      if (node.tagName === "IFRAME" || node.tagName === "FRAME") {
        try {
          const idoc = node.contentDocument;
          if (idoc && (idoc.body || idoc.documentElement)) {
            observeRoot(idoc);
            walk(idoc.body || idoc.documentElement, false);
            if (truncated) return;
          }
        } catch (_) {}
      }
      for (const ch of node.children) {
        walk(ch, nextSkipText);
        if (truncated) return;
      }
    }

    const root = document.body || document.documentElement;
    if (root) walk(root, false);

    const active = deepActiveElement();
    const focused = active && elToRef.has(active) ? elToRef.get(active) : null;
    return {
      url: location.href,
      title: document.title || "",
      version,
      scroll: scrollInfo(),
      count: lines.length,
      truncated,
      focused,
      lines
    };
  }

  // ---- Eylem ve olay yardımcıları ----
  function settleDom(quietMs = 150, maxMs = 1500) {
    const t0 = Date.now();
    return new Promise((resolve) => {
      let timer = null;
      const check = () => {
        const now = Date.now();
        const sinceMut = now - Math.max(t0, lastMutAt);
        const elapsed = now - t0;
        if (sinceMut >= quietMs || elapsed >= maxMs) {
          mutWaiters.delete(onMut);
          clearTimeout(timer);
          resolve();
          return;
        }
        clearTimeout(timer);
        timer = setTimeout(check, Math.max(10, Math.min(quietMs - sinceMut, maxMs - elapsed)));
      };
      const onMut = () => check();
      mutWaiters.add(onMut);
      timer = setTimeout(check, quietMs);
    });
  }

  async function finishAction(v0) {
    // Arka plan sekmesinde setTimeout >=1 sn'ye kısıldığından sakinleşmeyi background yürütür.
    if (document.hidden) {
      return {
        ok: true,
        url: location.href,
        title: document.title || "",
        version,
        navigated: false,
        changed: version !== v0,
        settle: "background"
      };
    }
    await settleDom(150, 1500);
    return {
      ok: true,
      url: location.href,
      title: document.title || "",
      version,
      navigated: false,
      changed: version !== v0
    };
  }

  function elemCenter(el) {
    const r = el.getBoundingClientRect();
    return { cx: r.left + r.width / 2, cy: r.top + r.height / 2 };
  }

  function fireMouse(el, type, cx, cy, bubbles = true, buttons = 0) {
    const win = el.ownerDocument.defaultView || window;
    const init = {
      bubbles,
      cancelable: bubbles,
      composed: true,
      view: win,
      clientX: cx,
      clientY: cy,
      button: 0,
      buttons
    };
    const isPtr = type.startsWith("pointer") && typeof win.PointerEvent === "function";
    const ev = isPtr
      ? new win.PointerEvent(type, Object.assign(init, { pointerId: 1, pointerType: "mouse", isPrimary: true }))
      : new win.MouseEvent(type, init);
    return el.dispatchEvent(ev);
  }

  const KEY_CODES = {
    Enter: 13, Escape: 27, Tab: 9, Backspace: 8, Delete: 46,
    ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40,
    Home: 36, End: 35, PageUp: 33, PageDown: 34, " ": 32
  };

  function fireKeySequence(el, keyName) {
    const win = el.ownerDocument.defaultView || window;
    const code = keyName === " " ? "Space"
      : (/^[a-z]$/i.test(keyName) ? "Key" + keyName.toUpperCase() : keyName);
    const keyCode = KEY_CODES[keyName] || (keyName.length === 1 ? keyName.toUpperCase().charCodeAt(0) : 0);
    const base = {
      key: keyName, code, keyCode, which: keyCode,
      bubbles: true, cancelable: true, composed: true, view: win
    };
    const okDown = el.dispatchEvent(new win.KeyboardEvent("keydown", base));
    if (keyName === "Enter" || keyName.length === 1) {
      el.dispatchEvent(new win.KeyboardEvent("keypress", Object.assign({}, base, { charCode: keyCode })));
    }
    el.dispatchEvent(new win.KeyboardEvent("keyup", base));
    return okDown;
  }

  function setNativeValue(el, val) {
    const win = el.ownerDocument.defaultView || window;
    const proto = el.tagName === "TEXTAREA"
      ? win.HTMLTextAreaElement.prototype
      : el.tagName === "SELECT" ? win.HTMLSelectElement.prototype : win.HTMLInputElement.prototype;
    const desc = Object.getOwnPropertyDescriptor(proto, "value");
    if (desc && desc.set) desc.set.call(el, val);
    else el.value = val;
  }

  function emitInputChange(el, text) {
    const win = el.ownerDocument.defaultView || window;
    try {
      el.dispatchEvent(new win.InputEvent("input", { bubbles: true, composed: true, inputType: "insertText", data: text }));
    } catch (_) {
      el.dispatchEvent(new win.Event("input", { bubbles: true }));
    }
    el.dispatchEvent(new win.Event("change", { bubbles: true }));
  }

  // ---- Sayfa op'ları ----
  async function opClick(args) {
    const el = resolveRef(args.ref);
    const v0 = version;
    el.scrollIntoView({ block: "center", inline: "nearest" });
    const { cx, cy } = elemCenter(el);
    fireMouse(el, "pointerdown", cx, cy, true, 1);
    const mdOk = fireMouse(el, "mousedown", cx, cy, true, 1);
    if (mdOk && typeof el.focus === "function") el.focus({ preventScroll: true });
    fireMouse(el, "pointerup", cx, cy, true, 0);
    fireMouse(el, "mouseup", cx, cy, true, 0);
    fireMouse(el, "click", cx, cy, true, 0);
    return finishAction(v0);
  }

  async function opType(args) {
    const el = resolveRef(args.ref);
    if (el.disabled) throw new Error("eleman devre dışı");
    const text = args.text === undefined || args.text === null ? "" : String(args.text);
    const clear = args.clear !== false;
    const v0 = version;
    const valOf = () => (el.isContentEditable ? (el.innerText || "") : (el.value || ""));
    const before = valOf();
    el.scrollIntoView({ block: "center", inline: "nearest" });
    el.focus({ preventScroll: true });

    if (el.isContentEditable) {
      const doc = el.ownerDocument;
      const sel = doc.getSelection();
      const range = doc.createRange();
      range.selectNodeContents(el);
      if (!clear) range.collapse(false);
      sel.removeAllRanges();
      sel.addRange(range);
      const ok = clear && text === ""
        ? doc.execCommand("delete", false)
        : doc.execCommand("insertText", false, text);
      if (!ok) {
        el.textContent = clear ? text : (el.textContent || "") + text;
        emitInputChange(el, text);
      }
    } else if (el.tagName === "INPUT" || el.tagName === "TEXTAREA") {
      setNativeValue(el, clear ? text : (el.value || "") + text);
      emitInputChange(el, text);
    } else {
      throw new Error("yazılabilir eleman değil: " + args.ref);
    }

    if (args.submit) {
      const notPrevented = fireKeySequence(el, "Enter");
      if (notPrevented && el.form) {
        if (typeof el.form.requestSubmit === "function") el.form.requestSubmit();
        else el.form.submit();
      }
    }
    // Değer değişimi DOM mutasyonu üretmeyebileceğinden sonucu önceki değerle karşılaştır.
    const res = await finishAction(v0);
    const after = valOf();
    if (after !== before) res.changed = true;
    if (!isSecretField(el)) res.value = after;
    return res;
  }

  async function opSelect(args) {
    const el = resolveRef(args.ref);
    if (el.tagName !== "SELECT") throw new Error("select elemanı değil: " + args.ref);
    if (el.disabled) throw new Error("eleman devre dışı");
    const target = String(args.option ?? "");
    const tNorm = target.trim().toLowerCase();
    const opts = Array.from(el.options || []);
    const opt = opts.find((o) => o.value === target || o.text.trim() === target.trim())
      || opts.find((o) => o.value.toLowerCase() === tNorm || o.text.trim().toLowerCase() === tNorm);
    if (!opt) throw new Error("seçenek bulunamadı: " + target);
    if (opt.disabled) throw new Error("seçenek devre dışı: " + target);
    const v0 = version;
    const beforeVal = el.value;
    const beforeIdx = el.selectedIndex;
    el.focus({ preventScroll: true });
    setNativeValue(el, opt.value);
    opt.selected = true;
    emitInputChange(el, opt.value);
    // Seçim değişimi DOM mutasyonu üretmeyebileceğinden önceki seçimle karşılaştır.
    const res = await finishAction(v0);
    if (el.value !== beforeVal || el.selectedIndex !== beforeIdx) res.changed = true;
    if (!isSecretField(el)) res.value = el.value;
    return res;
  }

  async function opCheck(args) {
    const el = resolveRef(args.ref);
    if (el.disabled) throw new Error("eleman devre dışı");
    const isNative = el.tagName === "INPUT" && (el.type === "checkbox" || el.type === "radio");
    if (!isNative) {
      const role = (el.getAttribute("role") || "").toLowerCase();
      if (role !== "checkbox" && role !== "switch" && role !== "radio") {
        throw new Error("onay kutusu elemanı değil: " + args.ref);
      }
    }
    const isChecked = () => (isNative ? Boolean(el.checked) : el.getAttribute("aria-checked") === "true");
    const before = isChecked();
    const on = Boolean(args.on);
    const v0 = version;
    el.scrollIntoView({ block: "center", inline: "nearest" });
    if (isChecked() !== on) {
      if (isNative && el.type === "radio" && !on) {
        el.checked = false;
        emitInputChange(el, "");
      } else {
        el.click();
      }
    }
    // İşaret değişimi DOM mutasyonu üretmeyebileceğinden önceki durumla karşılaştır.
    const res = await finishAction(v0);
    const after = isChecked();
    if (after !== before) res.changed = true;
    if (!isSecretField(el)) res.value = after;
    return res;
  }

  async function opHover(args) {
    const el = resolveRef(args.ref);
    const v0 = version;
    el.scrollIntoView({ block: "center", inline: "nearest" });
    const { cx, cy } = elemCenter(el);
    fireMouse(el, "pointerover", cx, cy, true, 0);
    fireMouse(el, "mouseover", cx, cy, true, 0);
    fireMouse(el, "pointerenter", cx, cy, false, 0);
    fireMouse(el, "mouseenter", cx, cy, false, 0);
    fireMouse(el, "pointermove", cx, cy, true, 0);
    fireMouse(el, "mousemove", cx, cy, true, 0);
    return finishAction(v0);
  }

  async function opFocus(args) {
    const el = resolveRef(args.ref);
    const v0 = version;
    el.scrollIntoView({ block: "center", inline: "nearest" });
    if (typeof el.focus === "function") el.focus({ preventScroll: true });
    return finishAction(v0);
  }

  async function opKey(args) {
    if (typeof args.key !== "string" || !args.key) throw new Error("key gerekli");
    const el = args.ref ? resolveRef(args.ref) : (deepActiveElement() || document.body || document.documentElement);
    if (!el) throw new Error("odaklanacak eleman yok");
    const v0 = version;
    if (args.ref && typeof el.focus === "function") el.focus({ preventScroll: true });
    const notPrevented = fireKeySequence(el, args.key);
    if (notPrevented && args.key === "Enter") {
      if (el.form && (el.tagName === "INPUT" || el.tagName === "SELECT")) {
        if (typeof el.form.requestSubmit === "function") el.form.requestSubmit();
        else el.form.submit();
      } else if (el.tagName === "A" || el.tagName === "BUTTON") {
        el.click();
      }
    }
    return finishAction(v0);
  }

  async function opScroll(args) {
    if (args.ref) {
      resolveRef(args.ref).scrollIntoView({ block: "center", inline: "nearest" });
    } else if (args.to === "top") {
      window.scrollTo(window.scrollX, 0);
    } else if (args.to === "bottom") {
      const sh = (document.documentElement || document.body || {}).scrollHeight || 0;
      window.scrollTo(window.scrollX, sh);
    } else if (args.dy !== undefined && args.dy !== null) {
      window.scrollBy(0, numOr(args.dy, 0));
    }
    await new Promise((r) => setTimeout(r, 30));
    return scrollInfo();
  }

  function opText(args) {
    const max = Math.max(1, numOr(args.max, 8000));
    const el = args.ref ? resolveRef(args.ref) : (document.body || document.documentElement);
    const raw = safeInnerText(el);
    if (raw.length > max) return { text: raw.slice(0, max), truncated: true };
    return { text: raw, truncated: false };
  }

  function opRect(args) {
    const el = resolveRef(args.ref);
    let r = topViewportRect(el);
    const cx0 = r.left + r.width / 2;
    const cy0 = r.top + r.height / 2;
    if (isOffscreen(r) || cx0 < 0 || cy0 < 0 || cx0 >= window.innerWidth || cy0 >= window.innerHeight) {
      el.scrollIntoView({ block: "center", inline: "center" });
      r = topViewportRect(el);
    }
    const chrome_x = Math.round((window.outerWidth - window.innerWidth) / 2);
    const chrome_y = Math.round(window.outerHeight - window.innerHeight - chrome_x);
    return {
      x: Math.round(r.left),
      y: Math.round(r.top),
      w: Math.round(r.width),
      h: Math.round(r.height),
      cx: Math.round(r.left + r.width / 2),
      cy: Math.round(r.top + r.height / 2),
      chrome_x,
      chrome_y,
      dpr: window.devicePixelRatio || 1,
      zoom: 1
    };
  }

  function opWait(args) {
    const total = Math.max(0, numOr(args.timeout_ms, 5000));
    const wantText = typeof args.text === "string" && args.text ? args.text : "";
    const wantGone = typeof args.gone === "string" && args.gone ? args.gone : "";
    const wantUrl = typeof args.url === "string" && args.url ? args.url : "";
    const t0 = Date.now();

    const isMatched = () => {
      if (wantUrl && !location.href.includes(wantUrl)) return false;
      if (wantText || wantGone) {
        const body = document.body || document.documentElement;
        const txt = body ? safeInnerText(body) : "";
        if (wantText && !txt.includes(wantText)) return false;
        if (wantGone && txt.includes(wantGone)) return false;
      }
      return true;
    };

    if (isMatched() || total === 0) {
      return Promise.resolve({ matched: isMatched(), elapsed_ms: Date.now() - t0, url: location.href });
    }

    return new Promise((resolve) => {
      let done = false;
      let timer = null;
      let tick = null;
      const finish = (matched) => {
        if (done) return;
        done = true;
        mutWaiters.delete(check);
        clearTimeout(timer);
        clearInterval(tick);
        resolve({ matched, elapsed_ms: Date.now() - t0, url: location.href });
      };
      const check = () => {
        if (isMatched()) finish(true);
      };
      mutWaiters.add(check);
      tick = setInterval(check, 100);
      timer = setTimeout(() => finish(isMatched()), total);
    });
  }

  // ---- İleti yönlendirici ----
  async function dispatch(op, args) {
    switch (op) {
      case "version": return version;
      case "ver": return { version, url: location.href, title: document.title || "" };
      case "snapshot": return opSnapshot(args);
      case "click": return opClick(args);
      case "type": return opType(args);
      case "select": return opSelect(args);
      case "check": return opCheck(args);
      case "hover": return opHover(args);
      case "focus": return opFocus(args);
      case "key": return opKey(args);
      case "scroll": return opScroll(args);
      case "text": return opText(args);
      case "rect": return opRect(args);
      case "wait": return opWait(args);
      default: throw new Error("bilinmeyen sayfa op'u: " + op);
    }
  }

  browser.runtime.onMessage.addListener((msg) => {
    if (!msg || typeof msg !== "object" || typeof msg.op !== "string") return undefined;
    const args = msg.args && typeof msg.args === "object" ? msg.args : {};
    return (async () => {
      try {
        const result = await dispatch(msg.op, args);
        return { ok: true, result };
      } catch (e) {
        return { ok: false, error: errText(e) };
      }
    })();
  });
})();
