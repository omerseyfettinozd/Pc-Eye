(() => {
  "use strict";
  if (window.__pcEyeBridge) return;
  if (window !== window.top) {
    let sameOrigin = false;
    try {
      sameOrigin = Boolean(window.parent.document);
    } catch (_) {
      sameOrigin = false;
    }
    // Aynı kaynaklı alt çerçeveyi üst çerçevenin betiği contentDocument ile zaten yürür.
    if (sameOrigin) return;
  }
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
    "menuitemradio", "slider", "spinbutton", "treeitem", "draggable", "file"
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
    if (ac && ac.split(/\s+/).some((t) =>
      t.startsWith("cc-") || t === "current-password" || t === "new-password" || t === "one-time-code"
    )) {
      return true;
    }
    // CSS ile maskelenen alanlar da gizli sayılır; maliyet için yalnız INPUT/TEXTAREA'da denenir.
    if (el.tagName === "INPUT" || el.tagName === "TEXTAREA") {
      try {
        const cs = (el.ownerDocument.defaultView || window).getComputedStyle(el);
        const ts = (cs.getPropertyValue("-webkit-text-security") || cs.webkitTextSecurity || "").trim();
        if (ts && ts !== "none") return true;
        if ((cs.fontFamily || "").toLowerCase().includes("text-security")) return true;
      } catch (_) {}
    }
    return false;
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
      // Dosya girişi düğme gibi görünürse ajan tıklamaya ya da ayrıca aramaya yönelir.
      if (tp === "file") return "file";
      if (tp === "button" || tp === "submit" || tp === "reset" || tp === "image") return "button";
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
    // Sürüklenebilir kartlar çoğu sitede rolsüzdür; ref alsınlar ki `drag` ile taşınabilsinler.
    if (el.getAttribute("draggable") === "true") return "draggable";
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
    // Seçim/basım/geçerli sayfa/açılır menü durumları ajanın sayfayı doğru okuması için eklenir.
    if (el.getAttribute("aria-selected") === "true" ||
        (el.tagName === "OPTION" && el.selected)) {
      flags.push("[selected]");
    }
    if (el.getAttribute("aria-pressed") === "true") flags.push("[pressed]");
    const ariaCurrent = el.getAttribute("aria-current");
    if (ariaCurrent !== null && ariaCurrent !== "false") flags.push("[current]");
    const ariaPopup = el.getAttribute("aria-haspopup");
    if (ariaPopup !== null && ariaPopup !== "false") flags.push("[popup]");
    // Kapalı menü açık sanılmasın diye ayrı bayrak verilir.
    if (el.getAttribute("aria-expanded") === "false") flags.push("[collapsed]");
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

  // Sorguyla tek satır döndüğünde sütun adları kaybolmasın: "Ürün kodu: X | Stok: 12".
  const tableHeads = new WeakMap();
  function labeledRowText(el) {
    const table = el.closest ? el.closest("table,[role=table],[role=grid]") : null;
    if (!table) return rowText(el);
    let heads = tableHeads.get(table);
    if (heads === undefined) {
      const hr = table.querySelector("tr");
      heads = hr && hr.querySelector("th,[role=columnheader]")
        ? Array.from(hr.children).map((c) => clip(safeInnerText(c), 30))
        : null;
      tableHeads.set(table, heads);
    }
    const cells = Array.from(el.children).filter((ch) => {
      const r = (ch.getAttribute("role") || "").toLowerCase();
      return ch.tagName === "TD" || ch.tagName === "TH" || r === "cell" || r === "rowheader";
    });
    if (!heads || heads.length !== cells.length || el.querySelector("th,[role=columnheader]")) return rowText(el);
    return cells.map((c, i) => {
      const v = clip(safeInnerText(c), 60);
      return heads[i] ? heads[i] + ": " + v : v;
    }).join(" | ");
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
        const allOpts = Array.from(el.options || []);
        const opts = allOpts.slice(0, 12).map((o) => clip(o.text, 30)).filter(Boolean);
        // Kesilen seçenek sayısı yazılır ki liste eksik sanılmasın.
        if (allOpts.length > 12) opts.push("+" + (allOpts.length - 12));
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
    let line = `[${ref}] ${role || "group"}`;
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
    // Kapsam verilirse yürüyüş o kökten başlar; ref (e12) ya da CSS seçici kabul edilir.
    const scope = typeof args.scope === "string" && args.scope.trim() ? args.scope.trim() : "";
    let scopeEl = null;
    if (scope) {
      if (/^e\d+$/.test(scope)) {
        try {
          scopeEl = resolveRef(scope);
        } catch (_) {
          throw new Error("scope bulunamadı: " + scope);
        }
      } else {
        try {
          scopeEl = document.querySelector(scope);
        } catch (_) {
          throw new Error("geçersiz scope seçicisi: " + scope);
        }
        if (!scopeEl) throw new Error("scope bulunamadı: " + scope);
      }
    }
    const lines = [];
    let truncated = false;
    let prev = null;

    // Sorgu bir başlıkla eşleşirse o başlığın bölümündeki satırlar da gelir ("X bölümünde ne var" tek çağrı).
    // Bölüm: başlığın küçük bir kabı varsa o kap; yoksa aynı/üst düzey sonraki başlığa kadar olan satırlar.
    let sectionLvl = 0;
    let sectionRoot = null;
    let sectionLeft = 0;
    let curNode = null;
    const pushLine = (entry, tr) => {
      if (viewportOnly && isOffscreen(tr)) return true;
      if (query) {
        const hit = entry.search.includes(query);
        if (entry.headingLevel && sectionLvl && entry.headingLevel <= sectionLvl) sectionLvl = 0;
        if (hit && entry.headingLevel) {
          const par = curNode ? curNode.parentElement : null;
          const boxed = par && par !== document.body && par.tagName !== "MAIN" && par.tagName !== "HTML" &&
            par.querySelectorAll("*").length <= 60;
          sectionRoot = boxed ? par : null;
          sectionLvl = boxed ? 0 : entry.headingLevel;
          sectionLeft = 25;
        } else if (!hit) {
          const inSection = sectionRoot ? Boolean(curNode && sectionRoot.contains(curNode)) : sectionLvl > 0;
          if (!inSection || sectionLeft <= 0) return true;
          sectionLeft--;
        }
      }
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
      curNode = node;
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
          if (t && !pushLine({ line: `heading(${lvl}) ${qStr(t, 120)}`, search: `heading ${t}`.toLowerCase(), headingLevel: Number(lvl) || 2 }, tr)) return;
          nextSkipText = true;
        } else if (role === "row") {
          const rt = query ? labeledRowText(node) : rowText(node);
          if (rt && !pushLine({ line: `row ${qStr(rt, 160)}`, search: `row ${rt}`.toLowerCase() }, tr)) return;
          nextSkipText = true;
        } else if (node.hasAttribute("tabindex") && node.tabIndex >= 0) {
          // Rolü etkileşimli olmasa da odaklanabilen kaplar (bırakma hedefi, özel bileşen) ref alır.
          if (!pushLine(formatInteractive(node, role, tr), tr)) return;
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

      // Rolsüz kaplardaki düz metin: `all` ile dökülür; sorguyla eşleşirse ref de alır (tıklanabilir, bırakma hedefi olabilir).
      if ((all || query) && selfVisible && hasBox && !nextSkipText && !role &&
          !BLOCK_TEXT_TAGS.has(node.tagName) && node.tagName !== "LABEL") {
        let own = "";
        for (const c of node.childNodes) if (c.nodeType === 3) own += c.nodeValue;
        own = norm(own);
        if (own.length >= 2) {
          const hit = query && own.toLowerCase().includes(query);
          if (all || hit) {
            const line = hit ? `[${getRef(node)}] text ${qStr(own, 120)}` : `text ${qStr(own, 120)}`;
            if (!pushLine({ line, search: `text ${own}`.toLowerCase() }, tr)) return;
          }
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

    const root = scopeEl || document.body || document.documentElement;
    // İzleme pikseli gibi görünmez alt çerçeveler özeti kirletmesin.
    const tinyFrame = window !== window.top && (window.innerWidth < 40 || window.innerHeight < 40);
    if (root && !tinyFrame) walk(root, false);

    const active = deepActiveElement();
    const focused = active && elToRef.has(active) ? elToRef.get(active) : null;
    const res = {
      url: location.href,
      title: document.title || "",
      version,
      scroll: scrollInfo(),
      count: lines.length,
      truncated,
      focused,
      lines
    };
    if (scope) res.scope = scope;
    return res;
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

  // Tıklama noktasını örten farklı bir eleman varsa kısa açıklamasını döndürür; örtülme yoksa null.
  function coveringElement(el, cx, cy) {
    if (!el || el.ownerDocument !== document) return null;
    if (cx < 0 || cy < 0 || cx >= window.innerWidth || cy >= window.innerHeight) return null;
    let hit = document.elementFromPoint(cx, cy);
    let steps = 0;
    // Gölge kökünde gerçek hedefe inmek için en fazla 10 adım ilerle.
    while (hit && hit.shadowRoot && typeof hit.shadowRoot.elementFromPoint === "function" && steps < 10) {
      const inner = hit.shadowRoot.elementFromPoint(cx, cy);
      if (!inner || inner === hit) break;
      hit = inner;
      steps++;
    }
    if (!hit) return null;
    if (hit === el || el.contains(hit) || hit.contains(el)) return null;
    let root = hit.getRootNode ? hit.getRootNode() : null;
    while (root && root !== document && root.host) {
      if (root.host === el || el.contains(root.host)) return null;
      root = root.host.getRootNode ? root.host.getRootNode() : null;
    }
    if (el.tagName === "INPUT") {
      const lbl = hit.closest ? hit.closest("label") : null;
      if (lbl && lbl.control === el) return null;
    }
    const tag = hit.tagName ? hit.tagName.toLowerCase() : "?";
    const text = clip(safeInnerText(hit), 40);
    return tag + (text ? ' "' + text + '"' : "");
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
    // Örtülme denetimi tıklamadan önce yapılır; tıklama yine gönderilir, sonuca not düşülür.
    const coveredBy = coveringElement(el, cx, cy);
    fireMouse(el, "pointerdown", cx, cy, true, 1);
    const mdOk = fireMouse(el, "mousedown", cx, cy, true, 1);
    if (mdOk && typeof el.focus === "function") el.focus({ preventScroll: true });
    fireMouse(el, "pointerup", cx, cy, true, 0);
    fireMouse(el, "mouseup", cx, cy, true, 0);
    fireMouse(el, "click", cx, cy, true, 0);
    const res = await finishAction(v0);
    if (coveredBy) res.covered_by = coveredBy;
    return res;
  }

  // ref verilmezse odaktaki alana yazılır (ör. arama kutusuna tıkladıktan sonra `type`).
  function focusedEditable() {
    let el = document.activeElement;
    while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
    const ok = el && (el.isContentEditable || el.tagName === "TEXTAREA" ||
      (el.tagName === "INPUT" && !/^(button|submit|reset|checkbox|radio|file|image|hidden)$/i.test(el.type)));
    if (!ok) throw new Error("ref verilmedi ve odakta yazılabilir alan yok (ref ya da name verin)");
    return el;
  }

  async function opType(args) {
    const el = args.ref ? resolveRef(args.ref) : focusedEditable();
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

  // Görünen metni ya da aria-label'ı verilen dizeyle eşleşen elemanı bulur (tam eşleşme önce).
  function findByLabel(text) {
    const want = norm(text).toLowerCase();
    if (!want || !document.body) return null;
    let best = null;
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_ELEMENT);
    for (let n = walker.nextNode(); n; n = walker.nextNode()) {
      if (SKIP_TAGS.has(n.tagName)) continue;
      const al = norm(n.getAttribute("aria-label") || "").toLowerCase();
      let own = "";
      for (const c of n.childNodes) if (c.nodeType === 3) own += c.nodeValue;
      own = norm(own).toLowerCase();
      if (al === want || own === want) return n;
      if (!best && (al.includes(want) || own.includes(want))) best = n;
    }
    return best;
  }

  async function opDrag(args) {
    const src = resolveRef(args.ref);
    if (!args.to) throw new Error("to gerekli (hedef ref)");
    // Bırakma hedefi çoğu zaman rolsüz bir kaptır: ref yoksa CSS seçici de kabul edilir.
    let dst;
    if (/^e\d+$/.test(String(args.to))) {
      dst = resolveRef(args.to);
    } else {
      try {
        dst = document.querySelector(String(args.to));
      } catch (_) {
        dst = null;
      }
      if (!dst) dst = findByLabel(String(args.to));
      if (!dst) throw new Error("hedef bulunamadı: " + args.to);
    }
    if (src.ownerDocument !== dst.ownerDocument) throw new Error("kaynak ve hedef farklı çerçevede");
    const v0 = version;
    src.scrollIntoView({ block: "center", inline: "nearest" });
    if (isOffscreen(topViewportRect(dst))) dst.scrollIntoView({ block: "center", inline: "nearest" });
    // Kaydırma iki elemanın konumunu da değiştirebilir; merkezler en son ölçülür.
    const { cx: sx, cy: sy } = elemCenter(src);
    const { cx: tx, cy: ty } = elemCenter(dst);
    fireMouse(src, "pointerdown", sx, sy, true, 1);
    fireMouse(src, "mousedown", sx, sy, true, 1);
    const win = src.ownerDocument.defaultView || window;
    const dt = new win.DataTransfer();
    const fireDrag = (el, type, x, y) => {
      try {
        const ev = new win.DragEvent(type, {
          bubbles: true,
          cancelable: true,
          composed: true,
          view: win,
          clientX: x,
          clientY: y,
          dataTransfer: dt
        });
        return el.dispatchEvent(ev);
      } catch (_) {
        return true;
      }
    };
    fireDrag(src, "dragstart", sx, sy);
    // İşaretçi tabanlı kitaplıklar ara hareket bekler; 5 adımda hedefe yaklaşılır.
    for (let i = 1; i <= 5; i++) {
      const rx = sx + ((tx - sx) * i) / 6;
      const ry = sy + ((ty - sy) * i) / 6;
      fireMouse(dst, "pointermove", rx, ry, true, 1);
      fireMouse(dst, "mousemove", rx, ry, true, 1);
      await new Promise((r) => setTimeout(r, 16));
    }
    fireDrag(dst, "dragenter", tx, ty);
    fireDrag(dst, "dragover", tx, ty);
    fireDrag(dst, "drop", tx, ty);
    fireDrag(src, "dragend", tx, ty);
    fireMouse(dst, "pointerup", tx, ty, true, 0);
    fireMouse(dst, "mouseup", tx, ty, true, 0);
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
    let el;
    if (args.ref) {
      el = resolveRef(args.ref);
    } else {
      // Ana içerik kökü yeterince metin taşıyorsa gövde yerine onu okuyup gürültüyü azalt.
      const main = args.all ? null : document.querySelector("main, [role=main], article");
      el = main && safeInnerText(main).length >= 200 ? main : (document.body || document.documentElement);
    }
    const raw = safeInnerText(el);
    const query = typeof args.query === "string" ? args.query.trim() : "";
    let full = raw;
    let matches = -1;
    if (query) {
      // Eşleşen satırlar ile birer komşusu döner; yinelenen satır tek kez yazılır.
      // "a|b" birden çok seçenek demektir (ajanlar düzenli ifade gibi yazıyor).
      const qs = query.toLowerCase().split("|").map((x) => x.trim()).filter(Boolean);
      const lines = raw.split("\n");
      const keep = new Set();
      matches = 0;
      for (let i = 0; i < lines.length; i++) {
        const low = lines[i].toLowerCase();
        if (!qs.some((x) => low.includes(x))) continue;
        matches++;
        keep.add(i - 1);
        keep.add(i);
        keep.add(i + 1);
      }
      const out = [];
      const seen = new Set();
      for (const i of Array.from(keep).sort((a, b) => a - b)) {
        if (i < 0 || i >= lines.length || seen.has(lines[i])) continue;
        seen.add(lines[i]);
        out.push(lines[i]);
      }
      full = out.join("\n");
    }
    const res = { text: full.length > max ? full.slice(0, max) : full, truncated: full.length > max };
    if (res.truncated) res.total = full.length;
    if (matches >= 0) res.matches = matches;
    return res;
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
    // Sayfa bu kadar süre hiç değişmezse beklenen şey gelmeyecektir: süre dolmadan dönülür (0 = kapalı).
    const idleMs = Math.max(0, numOr(args.idle_ms, 0));
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
      const idleCheck = () => {
        check();
        if (done || !idleMs || document.readyState !== "complete") return;
        if (Date.now() - Math.max(t0, lastMutAt) < idleMs) return;
        done = true;
        mutWaiters.delete(check);
        clearTimeout(timer);
        clearInterval(tick);
        resolve({ matched: false, idle: true, elapsed_ms: Date.now() - t0, url: location.href });
      };
      mutWaiters.add(check);
      tick = setInterval(idleCheck, 100);
      timer = setTimeout(() => finish(isMatched()), total);
    });
  }

  // Dosya girişine DataTransfer ile dosya yerleştirip input/change olaylarını tetikler.
  async function opUpload(args) {
    const ref = args.ref;
    let el = resolveRef(ref);
    const isFileInput = (n) => Boolean(n) && n.tagName === "INPUT" && (n.type || "").toLowerCase() === "file";
    if (!isFileInput(el)) {
      // Ref bir sarmalayıcıya ya da etikete işaret ediyorsa içindeki dosya girişini bul.
      const lbl = el.closest ? el.closest("label") : null;
      const inner = (el.querySelector && el.querySelector('input[type="file"]')) ||
        (lbl && lbl.control) || null;
      if (!isFileInput(inner)) throw new Error("dosya girişi değil: " + ref);
      el = inner;
    }
    const list = Array.isArray(args.files) ? args.files : [];
    if (!list.length) throw new Error("dosya listesi boş: " + ref);
    const win = el.ownerDocument.defaultView || window;
    const dt = new win.DataTransfer();
    const names = [];
    for (const f of list) {
      const bin = atob(String((f && f.b64) || ""));
      const bytes = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      const file = new win.File([bytes], String((f && f.name) || "dosya"), {
        type: (f && f.type) || "application/octet-stream"
      });
      dt.items.add(file);
      names.push(file.name);
    }
    if (!el.multiple && list.length > 1) throw new Error("bu giriş tek dosya kabul eder");
    const v0 = version;
    el.files = dt.files;
    el.dispatchEvent(new win.Event("input", { bubbles: true }));
    el.dispatchEvent(new win.Event("change", { bubbles: true }));
    const res = await finishAction(v0);
    res.files = names;
    return res;
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
      case "drag": return opDrag(args);
      case "focus": return opFocus(args);
      case "key": return opKey(args);
      case "scroll": return opScroll(args);
      case "text": return opText(args);
      case "upload": return opUpload(args);
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
