// The bar's menus are `<details>`, which stay open until they are toggled. These behave as menus:
// opening one closes the others, clicking or tabbing anywhere else closes them, and Escape closes
// the open one and puts focus back on the menu it belongs to. Everything still works without this.
(function () {
  var MENU = ".doc-menu__details";

  function menus() {
    return Array.prototype.slice.call(document.querySelectorAll(MENU));
  }

  function close(except) {
    menus().forEach(function (menu) {
      if (menu !== except) menu.open = false;
    });
  }

  function within(target) {
    return target && target.closest ? target.closest(MENU) : null;
  }

  // A menu that has just opened is the only one open.
  document.addEventListener(
    "toggle",
    function (event) {
      var menu = event.target;
      if (menu.matches && menu.matches(MENU) && menu.open) close(menu);
    },
    true
  );

  document.addEventListener("click", function (event) {
    if (!within(event.target)) close(null);
  });

  document.addEventListener("focusin", function (event) {
    if (!within(event.target)) close(null);
  });

  document.addEventListener("keydown", function (event) {
    if (event.key !== "Escape") return;
    var open = menus().filter(function (menu) {
      return menu.open;
    });
    if (!open.length) return;
    var summary = open[0].querySelector("summary");
    close(null);
    if (summary) summary.focus();
  });
})();
