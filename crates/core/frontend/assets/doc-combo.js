// A field you can type into that offers what matches: the Catalogue's resources, wherever a form
// asks for one. The options come from the server through HTMX, so this only opens and closes the
// list, moves through it and puts what was chosen in the field. Without it the field is still a
// text box you can type a name into, and the options are still links to read.
//
// The markup a plugin writes:
//
//   <div class="doc-combo" data-doc-combo>
//     <input class="doc-input doc-combo__input" name="resource" role="combobox"
//            aria-expanded="false" aria-controls="resource-options" autocomplete="off"
//            hx-get="/p/resources/options" hx-trigger="focus, input changed delay:200ms"
//            hx-target="#resource-options" hx-swap="innerHTML" hx-params="q" />
//     <div class="doc-combo__list" id="resource-options" role="listbox" hidden></div>
//   </div>
//
// and each option is a <button data-value="Service:card-gateway">, or an <a href="..."> where
// choosing means going there, as the search in the header does.
//
// A field that takes several values separated by commas adds data-doc-combo-list to the root: a
// choice then replaces only the part being typed and leaves a comma ready for the next one, and
// the server is sent the whole field, so it can offer what is not named in it yet.
//
// A field that stands for something with an ID of its own — a person a form names by ID, say —
// keeps that ID out of sight: put an <input type="hidden" class="doc-combo__value" name="…"> in
// the combo, and the chosen option's data-value goes there while its data-label is what the field
// shows. Typing in the field afterwards clears the ID, since what is written is no longer what
// was chosen, and a form that has one submits nothing for it until somebody chooses again.
//
// An option carrying data-label in a combo with no hidden field is treated the same way, so an ID
// is never what somebody reads: the field shows the label, and when its form is sent the ID goes
// in its place — as long as the field still says exactly what was chosen. Anything typed instead,
// such as a template, is sent as it is. A field a page draws with a choice already made carries
// the same two attributes this sets: value="acolwill" data-doc-combo-shown="acolwill"
// data-doc-combo-chosen="<the ID>".
(function () {
  var COMBO = "[data-doc-combo]";
  var OPTION = ".doc-combo__option";

  function parts(combo) {
    return {
      field: combo.querySelector(".doc-combo__input"),
      list: combo.querySelector(".doc-combo__list"),
      hidden: combo.querySelector(".doc-combo__value"),
    };
  }

  function options(combo) {
    return Array.prototype.slice.call(combo.querySelectorAll(OPTION));
  }

  function open(combo, yes) {
    var found = parts(combo);
    if (!found.field || !found.list) return;
    var has = options(combo).length > 0 || found.list.textContent.trim() !== "";
    var showing = yes && has;
    found.list.hidden = !showing;
    found.field.setAttribute("aria-expanded", showing ? "true" : "false");
    if (!showing) mark(combo, -1);
  }

  function mark(combo, index) {
    var all = options(combo);
    var found = parts(combo);
    all.forEach(function (option, at) {
      var active = at === index;
      option.classList.toggle("doc-combo__option--active", active);
      option.setAttribute("aria-selected", active ? "true" : "false");
      if (active) {
        option.scrollIntoView({ block: "nearest" });
        if (option.id === "") option.id = "doc-combo-option-" + at;
        if (found.field) found.field.setAttribute("aria-activedescendant", option.id);
      }
    });
    if (index < 0 && found.field) found.field.removeAttribute("aria-activedescendant");
    combo.dataset.active = String(index);
  }

  function active(combo) {
    var index = parseInt(combo.dataset.active || "-1", 10);
    return isNaN(index) ? -1 : index;
  }

  // An option either fills the field (a button carrying data-value) or goes somewhere (a link).
  function choose(combo, option) {
    var found = parts(combo);
    if (!found.field || !option) return;
    var href = option.getAttribute("href");
    if (href) {
      open(combo, false);
      window.location.assign(href);
      return;
    }
    var value = option.getAttribute("data-value") || "";
    var several = combo.hasAttribute("data-doc-combo-list");
    if (found.hidden) {
      // The field shows what it is called; the form is given the ID behind it.
      found.hidden.value = value;
      var label = option.getAttribute("data-label");
      found.field.value = label === null ? value : label;
    } else if (option.hasAttribute("data-label") && !several) {
      // The field shows the name; its form sends the ID in its place (below).
      found.field.value = option.getAttribute("data-label");
      found.field.setAttribute("data-doc-combo-chosen", value);
      found.field.setAttribute("data-doc-combo-shown", found.field.value);
    } else if (several) {
      var comma = found.field.value.lastIndexOf(",");
      var before = comma < 0 ? "" : found.field.value.slice(0, comma + 1) + " ";
      found.field.value = before + value + ", ";
    } else {
      found.field.value = value;
    }
    open(combo, false);
    found.field.dispatchEvent(new Event("change", { bubbles: true }));
    found.field.focus();
    // More may follow, so ask again for what is left to choose from.
    if (several) found.field.dispatchEvent(new Event("input", { bubbles: true }));
  }

  // A form being sent puts each chosen ID in place of the name its field shows, unless what the
  // field says has changed since. A field still being typed in is left alone, since that is its
  // own picker asking for options by what is written.
  document.addEventListener(
    "formdata",
    function (event) {
      var form = event.target;
      if (!form || !form.querySelectorAll) return;
      form.querySelectorAll("[data-doc-combo-chosen]").forEach(function (field) {
        if (!field.name || field === document.activeElement) return;
        if (field.value === field.getAttribute("data-doc-combo-shown")) {
          event.formData.set(field.name, field.getAttribute("data-doc-combo-chosen"));
        }
      });
    },
    true
  );

  function within(target) {
    return target && target.closest ? target.closest(COMBO) : null;
  }

  document.addEventListener("click", function (event) {
    var combo = within(event.target);
    if (!combo) {
      // A click anywhere else closes whatever is open.
      document.querySelectorAll(COMBO).forEach(function (other) {
        open(other, false);
      });
      return;
    }
    var option = event.target.closest(OPTION);
    if (option) {
      // A link is left to the browser, which knows about new tabs and middle clicks.
      if (option.getAttribute("href")) {
        open(combo, false);
        return;
      }
      event.preventDefault();
      choose(combo, option);
    }
  });

  document.addEventListener("keydown", function (event) {
    var combo = within(event.target);
    if (!combo || !event.target.classList.contains("doc-combo__input")) return;
    var all = options(combo);
    if (event.key === "Escape") {
      open(combo, false);
      return;
    }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      if (!all.length) return;
      event.preventDefault();
      open(combo, true);
      var step = event.key === "ArrowDown" ? 1 : -1;
      mark(combo, (active(combo) + step + all.length) % all.length);
      return;
    }
    if (event.key === "Enter" && active(combo) >= 0 && !combo.querySelector(".doc-combo__list").hidden) {
      event.preventDefault();
      choose(combo, all[active(combo)]);
    }
  });

  // What is typed by hand is no longer what was chosen, so the ID behind it goes.
  document.addEventListener("input", function (event) {
    var combo = within(event.target);
    if (!combo || !event.target.classList.contains("doc-combo__input")) return;
    var found = parts(combo);
    if (found.hidden) found.hidden.value = "";
  });

  document.addEventListener("focusin", function (event) {
    var combo = within(event.target);
    document.querySelectorAll(COMBO).forEach(function (other) {
      if (other !== combo) open(other, false);
    });
  });

  // Options that have just arrived belong to an open list. HTMX 4 says `htmx:after:swap`, older
  // ones `htmx:afterSwap`; both are listened for so the component does not depend on the version.
  ["htmx:after:swap", "htmx:afterSwap"].forEach(function (name) {
    document.addEventListener(name, function (event) {
      var combo = within(event.target);
      if (!combo) return;
      mark(combo, -1);
      open(combo, true);
    });
  });
})();
