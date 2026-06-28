// Tabbed panels (`.doc-tabset`): drawn as links to panels that all show, so they read without
// script, and made into tabs here — one panel at a time, chosen by a click or the arrow keys.
(function () {
  function choose(tabs, chosen) {
    tabs.forEach(function (tab) {
      var on = tab === chosen;
      tab.setAttribute("aria-selected", on ? "true" : "false");
      tab.tabIndex = on ? 0 : -1;
      document.getElementById(tab.getAttribute("aria-controls")).hidden = !on;
    });
  }

  function enhance(tabset) {
    var list = tabset.querySelector(".doc-tabs__list");
    var tabs = Array.prototype.slice.call(list.querySelectorAll(".doc-tabs__tab"));
    if (!tabs.length) return;
    list.setAttribute("role", "tablist");
    tabs.forEach(function (tab, at) {
      var panel = document.getElementById(tab.getAttribute("href").slice(1));
      tab.parentNode.setAttribute("role", "presentation");
      tab.setAttribute("role", "tab");
      tab.id = tab.id || panel.id + "-tab";
      tab.setAttribute("aria-controls", panel.id);
      panel.setAttribute("role", "tabpanel");
      panel.setAttribute("aria-labelledby", tab.id);
      panel.tabIndex = 0;
      tab.addEventListener("click", function (event) {
        event.preventDefault();
        choose(tabs, tab);
      });
      tab.addEventListener("keydown", function (event) {
        var next = { ArrowLeft: at - 1, ArrowRight: at + 1, Home: 0, End: tabs.length - 1 }[event.key];
        if (next === undefined) return;
        event.preventDefault();
        var chosen = tabs[(next + tabs.length) % tabs.length];
        choose(tabs, chosen);
        chosen.focus();
      });
    });
    choose(tabs, tabs[0]);
    tabset.classList.add("doc-tabset--ready");
  }

  function enhanceAll(root) {
    if (!root || !root.querySelectorAll) return;
    root.querySelectorAll(".doc-tabset:not(.doc-tabset--ready)").forEach(enhance);
  }

  document.addEventListener("DOMContentLoaded", function () {
    enhanceAll(document);
  });
  ["htmx:afterSwap", "htmx:after:swap"].forEach(function (name) {
    document.addEventListener(name, function (event) {
      enhanceAll(event.target || document);
    });
  });
})();
