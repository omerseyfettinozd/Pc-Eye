"use strict";

(() => {
  if (window.__pcEyeHooks) return;
  window.__pcEyeHooks = true;

  const RATE_LIMIT = 20;
  const RATE_WINDOW_MS = 1000;

  // Varsayılan ret: ajan görmeden "silinsin mi?" gibi bir soru onaylanmasın.
  let policy = { accept: false, text: null };
  let winStart = 0;
  let winCount = 0;
  let dropped = 0;

  function safeStr(v) {
    try {
      if (v === undefined || v === null) return "";
      return String(v);
    } catch (e) {
      return "";
    }
  }

  function frameLabel() {
    try {
      if (window === window.top) return "top";
      return location.origin || "frame";
    } catch (e) {
      return "frame";
    }
  }

  function pageUrl() {
    try {
      return String(location.href || "").slice(0, 200);
    } catch (e) {
      return "";
    }
  }

  function sendEvent(payload) {
    try {
      const msg = Object.assign({
        hook: "event",
        frame: frameLabel(),
        url: pageUrl()
      }, payload);
      const p = browser.runtime.sendMessage(msg);
      if (p && typeof p.catch === "function") p.catch(() => {});
    } catch (e) {}
  }

  function emit(payload) {
    const now = Date.now();
    if (now - winStart >= RATE_WINDOW_MS) {
      if (dropped > 0) {
        const n = dropped;
        dropped = 0;
        winStart = now;
        winCount = 1;
        sendEvent({
          kind: "console",
          level: "warn",
          text: "pc-eye: " + n + " olay atlandı"
        });
      } else {
        winStart = now;
        winCount = 0;
      }
    }
    if (winCount >= RATE_LIMIT) {
      dropped += 1;
      return;
    }
    winCount += 1;
    sendEvent(payload);
  }

  function applyPolicy(p) {
    if (!p || typeof p !== "object") return;
    if (typeof p.accept === "boolean") policy.accept = p.accept;
    if (p.text === null || typeof p.text === "string") policy.text = p.text;
  }

  // Yalnız background'dan gelen politika iletilerini dinle; content.js ile karışma.
  try {
    browser.runtime.onMessage.addListener((msg) => {
      if (!msg || typeof msg !== "object" || !msg.hook) return undefined;
      if (msg.hook === "policy") applyPolicy(msg);
      return undefined;
    });
  } catch (e) {}

  try {
    const hello = browser.runtime.sendMessage({ hook: "hello" });
    if (hello && typeof hello.then === "function") {
      hello.then((res) => applyPolicy(res)).catch(() => {});
    }
  } catch (e) {}

  const pageWin = (() => {
    try {
      return window.wrappedJSObject || null;
    } catch (e) {
      return null;
    }
  })();

  function formatArg(x) {
    let s = "";
    try {
      if (typeof x === "string") {
        s = x;
      } else if (x !== null && typeof x === "object") {
        let isErr = false;
        try {
          isErr = (x instanceof Error) ||
            (pageWin && typeof pageWin.Error === "function" && x instanceof pageWin.Error) ||
            (typeof x.message === "string" && typeof x.stack === "string");
        } catch (e) {}
        if (isErr) {
          try {
            s = typeof x.message === "string" && x.message ? x.message : String(x);
          } catch (e) {
            s = "Error";
          }
        } else {
          try {
            s = String(x);
          } catch (e) {
            s = "[nesne]";
          }
        }
      } else {
        s = String(x);
      }
    } catch (e) {
      s = "[okunamadı]";
    }
    return s.length > 300 ? s.slice(0, 300) : s;
  }

  // Sayfa dünyası kancaları (Firefox Xray API).
  if (pageWin && typeof exportFunction === "function") {
    try {
      exportFunction(function (msg) {
        const text = safeStr(msg);
        emit({ kind: "dialog", type: "alert", message: text.slice(0, 300) });
        return undefined;
      }, pageWin, { defineAs: "alert" });

      exportFunction(function (msg) {
        const text = safeStr(msg);
        const answer = Boolean(policy.accept);
        emit({ kind: "dialog", type: "confirm", message: text.slice(0, 300), answer });
        return answer;
      }, pageWin, { defineAs: "confirm" });

      exportFunction(function (msg, defVal) {
        const text = safeStr(msg);
        const fallback = defVal === undefined || defVal === null ? "" : safeStr(defVal);
        const answer = policy.accept ? (policy.text !== null ? policy.text : fallback) : null;
        emit({
          kind: "dialog",
          type: "prompt",
          message: text.slice(0, 300),
          answer: typeof answer === "string" ? answer.slice(0, 300) : answer
        });
        return answer;
      }, pageWin, { defineAs: "prompt" });
    } catch (e) {}

    try {
      const pageConsole = pageWin.console;
      if (pageConsole) {
        const wrapConsole = (level, orig) => function () {
          if (orig) {
            try {
              orig.apply(pageConsole, arguments);
            } catch (e) {}
          }
          try {
            const parts = [];
            for (let i = 0; i < arguments.length; i += 1) {
              parts.push(formatArg(arguments[i]));
            }
            const text = parts.join(" ").slice(0, 500);
            emit({ kind: "console", level, text });
          } catch (e) {}
        };

        if (typeof pageConsole.error === "function") {
          const origError = pageConsole.error;
          exportFunction(wrapConsole("error", origError), pageConsole, { defineAs: "error" });
        }
        if (typeof pageConsole.warn === "function") {
          const origWarn = pageConsole.warn;
          exportFunction(wrapConsole("warn", origWarn), pageConsole, { defineAs: "warn" });
        }
      }
    } catch (e) {}
  }

  // Yakalanmamış hatalar ve kaynak yükleme hataları.
  try {
    window.addEventListener("error", (ev) => {
      try {
        const tgt = ev && ev.target;
        if (tgt && tgt !== window && tgt.nodeType === 1) {
          const tag = safeStr(tgt.tagName || "element").toLowerCase();
          const src = safeStr(tgt.currentSrc || tgt.src || tgt.href || "");
          const text = (src ? tag + " " + src : tag).slice(0, 500);
          emit({ kind: "console", level: "resource", text });
          return;
        }
        const msg = ev && ev.message ? safeStr(ev.message) : "Uncaught error";
        const file = ev && ev.filename ? safeStr(ev.filename) : "";
        const line = ev && ev.lineno ? Number(ev.lineno) : 0;
        const loc = file ? file + ":" + line : (line ? ":" + line : "");
        const text = (loc ? msg + " (" + loc + ")" : msg).slice(0, 500);
        emit({ kind: "console", level: "exception", text });
      } catch (e) {}
    }, true);
  } catch (e) {}

  try {
    window.addEventListener("unhandledrejection", (ev) => {
      try {
        const reason = formatArg(ev ? ev.reason : undefined);
        emit({
          kind: "console",
          level: "exception",
          text: ("Unhandled rejection: " + reason).slice(0, 500)
        });
      } catch (e) {}
    });
  } catch (e) {}
})();
