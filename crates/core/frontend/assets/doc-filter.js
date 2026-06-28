// A field that narrows a list already on the page, rather than asking the server again: whatever
// row's text does not hold every word typed is hidden, and a container's own empty message is
// shown once nothing is left. A page reaches for this by marking a field `data-doc-filter`, the
// list it narrows `data-filter-target` (a selector for whatever holds the rows) and, if the page
// has one, `data-filter-empty` (a selector for the message to show when nothing matches). Rows
// are `tr` by default; `data-filter-row` on the field names a different selector for them.
(function () {
  function words(text) {
    return text.trim().toLowerCase().split(/\s+/).filter(Boolean);
  }

  function target(field) {
    var selector = field.getAttribute("data-filter-target");
    return selector ? document.querySelector(selector) : null;
  }

  function rows(field) {
    var list = target(field);
    if (!list) return [];
    var selector = field.getAttribute("data-filter-row") || "tr";
    return Array.prototype.slice.call(list.querySelectorAll(selector));
  }

  function apply(field) {
    var asked = words(field.value);
    var found = rows(field);
    var shown = 0;
    found.forEach(function (row) {
      var text = row.textContent.toLowerCase();
      var kept = asked.every(function (word) {
        return text.indexOf(word) !== -1;
      });
      row.hidden = !kept;
      if (kept) shown += 1;
    });
    var emptySelector = field.getAttribute("data-filter-empty");
    var empty = emptySelector ? document.querySelector(emptySelector) : null;
    if (empty) empty.hidden = found.length === 0 || shown !== 0;
  }

  document.addEventListener("input", function (event) {
    var field = event.target.closest && event.target.closest("[data-doc-filter]");
    if (field) apply(field);
  });
})();
