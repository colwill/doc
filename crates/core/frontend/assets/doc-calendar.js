// The day and week grids are drawn for the whole 24 hours, as a diary is, so they open scrolled
// to the working day - or to the first event, when that is earlier. Without this the grid still
// works; it just starts at midnight.
(function () {
  var GRID = ".doc-cal__body";
  var DAY_STARTS = 8; // the hour a grid opens at when there is nothing earlier

  function slotHeight(grid) {
    var column = grid.querySelector(".doc-cal__column");
    if (!column) return 0;
    return column.getBoundingClientRect().height / 96;
  }

  // The quarter hour the first event of the grid starts at, or null when there is none.
  function firstEvent(grid) {
    var earliest = null;
    var events = grid.querySelectorAll(".doc-cal__event");
    for (var i = 0; i < events.length; i++) {
      var slot = events[i].className.match(/doc-cal__event--s(\d+)\b/);
      if (!slot) continue;
      var at = parseInt(slot[1], 10);
      if (earliest === null || at < earliest) earliest = at;
    }
    return earliest;
  }

  function open(grid) {
    if (grid.dataset.opened === "yes") return;
    var height = slotHeight(grid);
    if (!height) return;
    var start = DAY_STARTS * 4;
    var first = firstEvent(grid);
    if (first !== null) start = Math.min(start, Math.max(first - 2, 0));
    grid.scrollTop = start * height;
    grid.dataset.opened = "yes";
  }

  function openAll(root) {
    var grids = (root || document).querySelectorAll(GRID);
    for (var i = 0; i < grids.length; i++) open(grids[i]);
  }

  document.addEventListener("DOMContentLoaded", function () {
    openAll(document);
  });
  // A view swapped in by HTMX is a new grid, which opens the same way.
  ["htmx:after:swap", "htmx:afterSwap"].forEach(function (name) {
    document.addEventListener(name, function (event) {
      openAll(event.target);
    });
  });
})();
