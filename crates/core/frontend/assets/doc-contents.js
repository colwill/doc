// Contents beside a document (`details.doc-contents`): drawn open, so they show without script,
// and folded on a narrow screen, where they would push the page itself out of sight.
(function () {
  var narrow = window.matchMedia("(max-width: 48.0525em)");
  function fold(root) {
    if (!narrow.matches) return;
    root.querySelectorAll("details.doc-contents[open]").forEach(function (contents) {
      contents.removeAttribute("open");
    });
  }
  document.addEventListener("DOMContentLoaded", function () {
    fold(document);
  });
  ["htmx:afterSwap", "htmx:after:swap"].forEach(function (name) {
    document.addEventListener(name, function (event) {
      fold(event.target || document);
    });
  });
})();
