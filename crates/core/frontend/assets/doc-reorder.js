// Rows that can be put in order by dragging them, or from the keyboard with the up and down arrow
// keys on a row's handle. The order fields stay as they are and are renumbered on every move, so
// the form submits exactly as it does without this script, and a browser without it still works.
(function () {

  function rows(body) {
    return Array.prototype.slice.call(body.children).filter(function (row) {
      return row.tagName === "TR";
    });
  }

  function renumber(body) {
    rows(body).forEach(function (row, at) {
      var order = row.querySelector("[data-reorder-order]");
      if (order) order.value = at + 1;
    });
  }

  function named(row) {
    var handle = row.querySelector("[data-reorder-handle]");
    return (handle && handle.getAttribute("data-reorder-name")) || "The row";
  }

  function setup(body) {
    // A table can arrive more than once: an HTMX swap brings a new one, and a refused save sends
    // the same page back. Each is prepared once.
    if (body.hasAttribute("data-reorder-ready")) return;
    body.setAttribute("data-reorder-ready", "");
    var live = document.createElement("p");
    live.className = "nhsuk-u-visually-hidden";
    live.setAttribute("aria-live", "polite");
    body.parentNode.parentNode.insertBefore(live, body.parentNode.nextSibling);

    var dragging = null;

    function moved(row) {
      renumber(body);
      var all = rows(body);
      live.textContent =
        named(row) + " is now " + (all.indexOf(row) + 1) + " of " + all.length + ".";
    }

    function prepare(row) {
      row.setAttribute("draggable", "true");

      row.addEventListener("dragstart", function (event) {
        dragging = row;
        row.classList.add("doc-arrange__row--dragging");
        if (event.dataTransfer) {
          event.dataTransfer.effectAllowed = "move";
          try {
            event.dataTransfer.setData("text/plain", "");
          } catch (ignored) {}
        }
      });

      row.addEventListener("dragend", function () {
        row.classList.remove("doc-arrange__row--dragging");
        if (dragging) moved(dragging);
        dragging = null;
      });

      row.addEventListener("dragover", function (event) {
        if (!dragging || dragging === row) return;
        event.preventDefault();
        var box = row.getBoundingClientRect();
        var below = event.clientY - box.top > box.height / 2;
        body.insertBefore(dragging, below ? row.nextSibling : row);
      });

      row.addEventListener("drop", function (event) {
        event.preventDefault();
      });

      var handle = row.querySelector("[data-reorder-handle]");
      if (!handle) return;
      handle.addEventListener("keydown", function (event) {
        var up = event.key === "ArrowUp";
        var down = event.key === "ArrowDown";
        if (!up && !down) return;
        event.preventDefault();
        var neighbour = up ? row.previousElementSibling : row.nextElementSibling;
        if (!neighbour) return;
        if (up) body.insertBefore(row, neighbour);
        else body.insertBefore(neighbour, row);
        handle.focus();
        moved(row);
      });
    }

    rows(body).forEach(prepare);
  }

  function all(root) {
    (root || document).querySelectorAll("[data-reorder]").forEach(setup);
  }

  all();
  // A table can arrive with a page HTMX swaps in, such as an editor opened in place.
  ["htmx:after:swap", "htmx:afterSwap"].forEach(function (name) {
    document.addEventListener(name, function () {
      all();
    });
  });
})();
