// A builder: ready-made steps put on a chain of columns, by dragging them from a palette or with
// each one's own Add button. A drop only presses that button, so everything a drop can do the
// keyboard can do too, and the page's HTMX does the adding. Steps already on the chain move by a
// handle — dragged, or with the up and down arrow keys — and each has a form, of which one is
// shown at a time once this script runs. Without it every form is shown and each node links to its
// own, so the page still works. It is used by marking the chain `data-doc-builder`:
//
//   <div class="doc-builder" data-doc-builder>
//     <li data-doc-builder-item="then" draggable="true">… <button data-doc-builder-add hx-get=…>Add</button></li>
//     <ul data-doc-builder-zone="then">
//       <li data-doc-builder-step="s1a2b3c4d-"><span data-doc-builder-handle tabindex="0"></span>
//         <a data-doc-builder-select="s1a2b3c4d-" href="#step-s1a2b3c4d-">Post to Slack</a></li>
//     </ul>
//   </div>
//   <section data-doc-builder-form="s1a2b3c4d-">…<button data-doc-builder-remove="s1a2b3c4d-">…</section>
//   <p data-doc-builder-live aria-live="polite"></p>
//
// Everything is found through the form holding the builder, which is where the forms live too.
(function () {
  var dragging = null;

  function scopeOf(element) {
    var builder = element.closest("[data-doc-builder]");
    if (builder) return builder.closest("form") || builder;
    return element.closest("form") || document;
  }

  function quoted(value) {
    return '"' + (window.CSS && CSS.escape ? CSS.escape(value) : value) + '"';
  }

  function nodeOf(scope, prefix) {
    return scope.querySelector("[data-doc-builder-step=" + quoted(prefix) + "]");
  }

  function formOf(scope, prefix) {
    return scope.querySelector("[data-doc-builder-form=" + quoted(prefix) + "]");
  }

  function say(scope, text) {
    var region = scope.querySelector("[data-doc-builder-live]");
    if (region) region.textContent = text;
  }

  function title(step) {
    var node = step.querySelector("[data-doc-builder-select]");
    return node ? node.textContent.trim() : "The step";
  }

  function heading(step) {
    var column = step.closest("section");
    var named = column && column.querySelector("h1, h2, h3, h4, h5, h6");
    return named ? named.textContent.trim() : "the chain";
  }

  function steps(zone) {
    return Array.prototype.filter.call(zone.children, function (child) {
      return child.hasAttribute("data-doc-builder-step");
    });
  }

  // Shows one step's form and marks its node; the others' forms stay in the page, and are sent.
  function select(scope, prefix, focus) {
    scope.querySelectorAll("[data-doc-builder-form]").forEach(function (form) {
      form.hidden = form.getAttribute("data-doc-builder-form") !== prefix;
    });
    scope.querySelectorAll("[data-doc-builder-select]").forEach(function (node) {
      var chosen = node.getAttribute("data-doc-builder-select") === prefix;
      node.classList.toggle("doc-builder__node--selected", chosen);
      if (chosen) node.setAttribute("aria-current", "step");
      else node.removeAttribute("aria-current");
    });
    if (!focus) return;
    var form = formOf(scope, prefix);
    var field = form && form.querySelector("input:not([type=hidden]), select, textarea");
    if (field) field.focus();
  }

  // A form whose node has gone, such as a trigger another replaced, would still be sent.
  function tidy(scope) {
    scope.querySelectorAll("[data-doc-builder-form]").forEach(function (form) {
      if (!nodeOf(scope, form.getAttribute("data-doc-builder-form"))) form.remove();
    });
  }

  function setup(builder) {
    if (builder.hasAttribute("data-doc-builder-ready")) return;
    builder.setAttribute("data-doc-builder-ready", "");
    var scope = scopeOf(builder);
    // Steps already seen: a node moved within its column is removed and added again, and is not new.
    var known = {};
    scope.querySelectorAll("[data-doc-builder-step]").forEach(function (step) {
      known[step.getAttribute("data-doc-builder-step")] = true;
    });
    var marked = scope.querySelector("[data-doc-builder-form][data-doc-builder-selected]");
    var first = marked || scope.querySelector("[data-doc-builder-form]");
    select(scope, first ? first.getAttribute("data-doc-builder-form") : "", false);

    // A step arrives as its node where it was put and its form beside the others, in either
    // order; whichever comes last, the new step is the one shown.
    new MutationObserver(function (changes) {
      var newest = null;
      changes.forEach(function (change) {
        change.addedNodes.forEach(function (added) {
          if (added.nodeType !== 1) return;
          var prefix =
            added.getAttribute("data-doc-builder-step") ||
            added.getAttribute("data-doc-builder-form");
          if (prefix && !known[prefix]) newest = prefix;
        });
      });
      tidy(scope);
      if (!newest || !nodeOf(scope, newest)) return;
      select(scope, newest, Boolean(formOf(scope, newest)));
      if (formOf(scope, newest)) {
        known[newest] = true;
        var step = nodeOf(scope, newest);
        say(scope, title(step) + " added under " + heading(step) + ".");
      }
    }).observe(scope, { childList: true, subtree: true });
  }

  function moved(step) {
    var all = steps(step.parentNode);
    say(
      scopeOf(step),
      title(step) + " is now " + (all.indexOf(step) + 1) + " of " + all.length + " under " +
        heading(step) + "."
    );
  }

  function zones(scope, zone, on) {
    scope.querySelectorAll("[data-doc-builder-zone=" + quoted(zone) + "]").forEach(function (each) {
      each.classList.toggle("doc-builder__zone--accepts", on);
    });
  }

  function finish() {
    if (!dragging) return;
    var scope = scopeOf(dragging.element);
    scope.querySelectorAll(".doc-builder__zone--over, .doc-builder__zone--accepts").forEach(
      function (zone) {
        zone.classList.remove("doc-builder__zone--over", "doc-builder__zone--accepts");
      }
    );
    dragging.element.classList.remove("doc-builder__step--dragging");
    if (dragging.step) {
      dragging.element.removeAttribute("draggable");
      moved(dragging.element);
    }
    dragging = null;
  }

  // A step on the chain is dragged only by its handle, so its link still takes a click.
  document.addEventListener("pointerdown", function (event) {
    var handle = event.target.closest && event.target.closest("[data-doc-builder-handle]");
    if (!handle) return;
    var step = handle.closest("[data-doc-builder-step]");
    if (step) step.setAttribute("draggable", "true");
  });

  document.addEventListener("dragstart", function (event) {
    var target = event.target.closest && event.target.closest("[data-doc-builder-item], [data-doc-builder-step]");
    if (!target || !target.closest("[data-doc-builder]")) return;
    var step = target.hasAttribute("data-doc-builder-step");
    if (step && target.getAttribute("draggable") !== "true") return;
    dragging = {
      element: target,
      step: step,
      zone: step
        ? target.parentNode.getAttribute("data-doc-builder-zone")
        : target.getAttribute("data-doc-builder-item"),
    };
    if (step) target.classList.add("doc-builder__step--dragging");
    else zones(scopeOf(target), dragging.zone, true);
    if (event.dataTransfer) {
      event.dataTransfer.effectAllowed = step ? "move" : "copy";
      try {
        event.dataTransfer.setData("text/plain", target.textContent.trim());
      } catch (ignored) {}
    }
  });

  document.addEventListener("dragover", function (event) {
    if (!dragging) return;
    var zone = event.target.closest && event.target.closest("[data-doc-builder-zone]");
    if (!zone || zone.getAttribute("data-doc-builder-zone") !== dragging.zone) return;
    if (dragging.step && zone !== dragging.element.parentNode) return;
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = dragging.step ? "move" : "copy";
    if (!dragging.step) {
      zone.classList.add("doc-builder__zone--over");
      return;
    }
    var over = event.target.closest("[data-doc-builder-step]");
    if (!over || over === dragging.element) return;
    var box = over.getBoundingClientRect();
    var below = event.clientY - box.top > box.height / 2;
    zone.insertBefore(dragging.element, below ? over.nextSibling : over);
  });

  document.addEventListener("dragleave", function (event) {
    var zone = event.target.closest && event.target.closest("[data-doc-builder-zone]");
    if (zone && !zone.contains(event.relatedTarget)) zone.classList.remove("doc-builder__zone--over");
  });

  document.addEventListener("drop", function (event) {
    if (!dragging) return;
    var zone = event.target.closest && event.target.closest("[data-doc-builder-zone]");
    if (!zone || zone.getAttribute("data-doc-builder-zone") !== dragging.zone) return;
    event.preventDefault();
    if (!dragging.step) {
      var add = dragging.element.querySelector("[data-doc-builder-add]");
      if (add) add.click();
    }
    finish();
  });

  document.addEventListener("dragend", finish);

  document.addEventListener("keydown", function (event) {
    var handle = event.target.closest && event.target.closest("[data-doc-builder-handle]");
    if (!handle || (event.key !== "ArrowUp" && event.key !== "ArrowDown")) return;
    var step = handle.closest("[data-doc-builder-step]");
    var zone = step && step.parentNode;
    if (!zone) return;
    event.preventDefault();
    var up = event.key === "ArrowUp";
    var neighbour = up ? step.previousElementSibling : step.nextElementSibling;
    if (!neighbour || !neighbour.hasAttribute("data-doc-builder-step")) return;
    if (up) zone.insertBefore(step, neighbour);
    else zone.insertBefore(neighbour, step);
    handle.focus();
    moved(step);
  });

  document.addEventListener("click", function (event) {
    if (!event.target.closest) return;
    var chosen = event.target.closest("[data-doc-builder-select]");
    if (chosen && chosen.closest("[data-doc-builder]")) {
      event.preventDefault();
      select(scopeOf(chosen), chosen.getAttribute("data-doc-builder-select"), true);
      return;
    }
    var remove = event.target.closest("[data-doc-builder-remove]");
    if (!remove) return;
    var scope = scopeOf(remove);
    var prefix = remove.getAttribute("data-doc-builder-remove");
    var step = nodeOf(scope, prefix);
    var gone = step ? title(step) : "The step";
    var where = step ? heading(step) : "the chain";
    if (step) step.remove();
    var form = formOf(scope, prefix);
    if (form) form.remove();
    var next = scope.querySelector("[data-doc-builder-form]");
    select(scope, next ? next.getAttribute("data-doc-builder-form") : "", false);
    say(scope, gone + " removed from " + where + ".");
  });

  function all(root) {
    (root || document).querySelectorAll("[data-doc-builder]").forEach(setup);
  }

  all();
  // A builder can arrive with a page HTMX swaps in, such as one sent back when saving was refused.
  ["htmx:after:swap", "htmx:afterSwap"].forEach(function (name) {
    document.addEventListener(name, function () {
      all();
    });
  });
})();
