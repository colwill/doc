// Server-sent events from /events become `doc-<kind>` events on the body, which HTMX elements
// refresh on; finished tasks and plugin state changes also show as toasts.
(function () {
  if (!window.EventSource || !document.body.hasAttribute("data-live")) return;
  var toasts = document.querySelector(".doc-toasts");
  function toast(kind, title, text) {
    if (!toasts) return;
    var box = document.createElement("div");
    box.className = "doc-toast doc-toast--" + kind;
    var heading = document.createElement("p");
    heading.className = "doc-toast__title";
    heading.textContent = title;
    var content = document.createElement("div");
    content.className = "doc-toast__content";
    var line = document.createElement("p");
    line.textContent = text;
    content.appendChild(line);
    box.appendChild(heading);
    box.appendChild(content);
    toasts.appendChild(box);
    setTimeout(function () { box.remove(); }, 8000);
  }
  var finished = { succeeded: "success", failed: "error", cancelled: "info" };

  // The stream holds a connection for as long as it is open, and a browser allows only a handful
  // to one host (six in Chromium), counting every tab. A tab nobody is looking at gives its
  // connection back, so open tabs cannot use them all up and leave a page waiting to load.
  var source = null;

  function listen() {
    if (source) return;
    source = new EventSource("/events");
    ["status", "plugin-state", "task", "plugin-ui"].forEach(function (name) {
      source.addEventListener(name, function (message) {
        var detail = {};
        try { detail = JSON.parse(message.data); } catch (ignored) {}
        document.body.dispatchEvent(new CustomEvent("doc-" + name, { detail: detail }));
        if (detail.plugin) {
          document.body.dispatchEvent(new CustomEvent("doc-" + name + "-" + detail.plugin, { detail: detail }));
        }
        // A plugin's own scheduled or on-demand work (started_by.kind "plugin") finishes on a
        // timer or a button in that plugin's own UI, so it is noise here; a task a person queued
        // themselves (started_by.kind "user") is news.
        var own = detail.started_by && detail.started_by.kind === "plugin";
        if (name === "task" && finished[detail.state] && !own) {
          toast(finished[detail.state], "Task " + detail.state, detail.kind + (detail.error ? ": " + detail.error : ""));
        }
        if (name === "plugin-state") {
          toast(detail.state === "error" ? "error" : "info", detail.plugin, detail.state ? "is " + detail.state : "has stopped");
        }
      });
    });
    source.addEventListener("lagged", function () { location.reload(); });
  }

  function drop() {
    if (!source) return;
    source.close();
    source = null;
  }

  document.addEventListener("visibilitychange", function () {
    if (document.hidden) drop();
    else listen();
  });
  // A page put away in the back/forward cache lets its connection go with it.
  window.addEventListener("pagehide", drop);
  window.addEventListener("pageshow", function () {
    if (!document.hidden) listen();
  });

  if (!document.hidden) listen();
})();
