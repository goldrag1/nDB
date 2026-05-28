/* nDB feedback widget — vanilla JS, no deps.
   Injected by server.py into every served HTML page (including the
   proxied alphafold_ndb demo). Posts to /api/feedback which writes one
   nDB entity (type_id=200) per submission. */
(function () {
  "use strict";

  // Idempotency guard — script may be injected twice if a page reload
  // hits the proxy and the static path back-to-back during dev.
  if (window.__ndbFeedbackInit) return;
  window.__ndbFeedbackInit = true;

  // ── DOM scaffolding ──────────────────────────────────────────────
  function el(tag, props, children) {
    var node = document.createElement(tag);
    if (props) {
      for (var k in props) {
        if (k === "class") node.className = props[k];
        else if (k === "html") node.innerHTML = props[k];
        else if (k.indexOf("on") === 0) node.addEventListener(k.slice(2), props[k]);
        else node.setAttribute(k, props[k]);
      }
    }
    (children || []).forEach(function (c) {
      if (c == null) return;
      node.appendChild(typeof c === "string" ? document.createTextNode(c) : c);
    });
    return node;
  }

  var fab = el("button", { class: "ndb-fb-fab", type: "button", title: "Send feedback" }, [
    // Inline SVG so the icon survives even if the CSS file failed to load.
    el("span", {
      html:
        '<svg viewBox="0 0 24 24" aria-hidden="true">' +
        '<path d="M21 6.5v8A2.5 2.5 0 0 1 18.5 17h-7.1l-4.7 3.6c-.6.5-1.5.1-1.5-.7V17h-.7A2.5 2.5 0 0 1 2 14.5v-8A2.5 2.5 0 0 1 4.5 4h14A2.5 2.5 0 0 1 21 6.5z"/>' +
        "</svg>",
    }),
    "Feedback",
  ]);

  var nameInput    = el("input", { type: "text",  maxlength: "120", placeholder: "(optional)" });
  var emailInput   = el("input", { type: "email", maxlength: "200", placeholder: "(optional — only if you want a reply)" });
  var messageInput = el("textarea", { maxlength: "4000", placeholder: "What works, what doesn't, what's confusing…" });
  var statusLine   = el("div", { class: "ndb-fb-status" });
  var cancelBtn    = el("button", { type: "button", class: "ndb-fb-btn ghost" }, ["Cancel"]);
  var sendBtn      = el("button", { type: "button", class: "ndb-fb-btn" }, ["Send"]);

  var modal = el("div", { class: "ndb-fb-modal", role: "dialog", "aria-modal": "true" }, [
    el("h3", null, ["Send feedback"]),
    el("p", { class: "ndb-fb-sub" }, [
      "Anything you say lands as one nDB entity (type 200) on the same engine the alphafold demo runs on. Stored locally; not shared.",
    ]),
    el("label", null, ["Your name"]),     nameInput,
    el("label", null, ["Your email"]),    emailInput,
    el("label", null, ["Message *"]),     messageInput,
    el("div", { class: "ndb-fb-row" }, [
      statusLine,
      cancelBtn,
      sendBtn,
    ]),
    el("div", { class: "ndb-fb-foot" }, [
      "We dogfood our own DB: read more at ",
      el("a", { href: "/demos-alphafold_ndb.html" }, ["alphafold_ndb"]),
      ".",
    ]),
  ]);

  var backdrop = el("div", { class: "ndb-fb-backdrop", role: "presentation" }, [modal]);

  // ── open / close ─────────────────────────────────────────────────
  function open() {
    statusLine.textContent = "";
    statusLine.className = "ndb-fb-status";
    sendBtn.disabled = false;
    sendBtn.textContent = "Send";
    backdrop.classList.add("open");
    setTimeout(function () { messageInput.focus(); }, 30);
  }
  function close() {
    backdrop.classList.remove("open");
  }

  fab.addEventListener("click", open);
  cancelBtn.addEventListener("click", close);
  backdrop.addEventListener("click", function (e) {
    if (e.target === backdrop) close();
  });
  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape" && backdrop.classList.contains("open")) close();
  });

  // ── submit ───────────────────────────────────────────────────────
  function setStatus(text, kind) {
    statusLine.textContent = text;
    statusLine.className = "ndb-fb-status" + (kind ? " " + kind : "");
  }

  sendBtn.addEventListener("click", function () {
    var msg = (messageInput.value || "").trim();
    if (!msg) {
      setStatus("Message required.", "err");
      messageInput.focus();
      return;
    }
    sendBtn.disabled = true;
    sendBtn.textContent = "Sending…";
    setStatus("Writing entity to nDB…");

    fetch("/api/feedback", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        name:    nameInput.value.trim(),
        email:   emailInput.value.trim(),
        message: msg,
        // window.location.pathname tells the admin which page the
        // feedback came from — without leaking any PII.
        page:    location.pathname + location.search,
      }),
    })
      .then(function (r) {
        return r.json().then(function (j) { return { ok: r.ok, status: r.status, body: j }; });
      })
      .then(function (out) {
        if (!out.ok || !out.body.ok) {
          var err = (out.body && out.body.error) || ("HTTP " + out.status);
          setStatus("Failed: " + err, "err");
          sendBtn.disabled = false;
          sendBtn.textContent = "Send";
          return;
        }
        setStatus("Saved as nDB entity " + (out.body.id || "").slice(0, 8) + "… — thank you.", "ok");
        messageInput.value = "";
        nameInput.value = "";
        emailInput.value = "";
        sendBtn.textContent = "Sent";
        // Auto-close after a beat.
        setTimeout(close, 1400);
      })
      .catch(function (err) {
        setStatus("Network error: " + err, "err");
        sendBtn.disabled = false;
        sendBtn.textContent = "Send";
      });
  });

  // ── mount ────────────────────────────────────────────────────────
  function mount() {
    document.body.appendChild(fab);
    document.body.appendChild(backdrop);
  }
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", mount);
  } else {
    mount();
  }
})();
