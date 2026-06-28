// A canvas you arrange: things sit where you put them, the view pans under the right mouse button,
// and hovering a thing shows handles you drag from one to another to connect them. The page holds
// the truth — every move and every new line is an HTMX request, so a reload shows what you left,
// and nothing is kept only in the browser.
//
// Interaction, in the order a pointer meets it:
//
//   · left-drag a node            moves it; on release it is saved
//   · drag a node's bottom-right
//     grip                        resizes it; on release it is saved
//   · click a line                opens its menu: what it says, which way round, or remove it
//   · click a node                shows its × ; the × takes it off the view with its lines. Beside
//                                 it on the left, its edit button opens the panel for how it looks
//                                 on this view, and its expand button goes to the view it opens into
//   · drag from the palette       puts that thing on the canvas where it was dropped
//
// A view opens centred on its primary thing, named by `data-doc-canvas-primary`, zoomed out until
// everything else on it is in sight too, and Reset goes back there. Where somebody has panned and
// zoomed to is kept across the swaps a save makes, and so is a thing's panel, which sits beside it
// and saves each change as it is made.
//
// A node carries a connector a side, and a line remembers which side it was drawn from and to, so
// the shape somebody gave a map is the shape it keeps.
//   · right-drag (or middle), or
//     left-drag the background    pans the surface
//   · hover or focus a node       shows its connection handles
//   · drag from a handle onto
//     another node                connects them
//   · arrow keys on a focused
//     node                        moves it a step at a time, and saves
//
// The markup a plugin writes:
//
//   <div class="doc-canvas" data-doc-canvas
//        data-doc-canvas-place="/p/architecture/views/1/place"
//        data-doc-canvas-connect="/p/architecture/views/1/connect"
//        data-doc-canvas-target="#architecture">
//     <div class="doc-canvas__surface" data-doc-canvas-surface style="--pan-x: 0; --pan-y: 0">
//       <svg class="doc-canvas__lines" data-doc-canvas-lines></svg>
//       <div class="doc-canvas__node" data-doc-canvas-node="component:backend/api"
//            data-x="120" data-y="80" tabindex="0">
//         <span class="doc-canvas__name">API</span>
//         <span class="doc-canvas__handle" data-doc-canvas-handle></span>
//       </div>
//     </div>
//   </div>
//
// Positions arrive in `data-x`/`data-y` rather than a `style` attribute, because the page's CSP
// sets `style-src 'self'` with no `unsafe-inline`: an inline style attribute is dropped, while a
// custom property set from script is not. So this reads the attributes once and applies them.
//
// and each line it already knows about as
// `<span data-doc-canvas-line data-from="…" data-to="…" data-label="calls"></span>` anywhere
// inside the canvas. Positions are in grid steps, not pixels, so what is saved is small, stable
// and the same whatever the window is doing.
(function () {
  var STEP = 10;
  var SNAP = 2;
  // A grid square is the step things snap to. Sizes match a neighbour's within NEAR squares, and
  // let go once dragged FREE squares past it, so the match is sticky rather than magnetic.
  var SQUARE = SNAP;
  var NEAR = 10 * SQUARE;
  var FREE = 2 * SQUARE;
  // How far the zoom goes either way, and the room kept round the edge when a view opens fitted.
  var LEAST = 0.2;
  var MOST = 2.5;
  var MARGIN = 24;
  var dragging = null;
  var sizing = null;
  var linking = null;
  var panning = null;
  // Something carried from the palette onto the canvas.
  var carrying = null;
  // Where each canvas was panned and zoomed to, by view, so the swap every save makes does not
  // throw it back to where the view opens. A fresh page starts again at the primary.
  var kept = {};
  // The thing whose panel is open, by view, so it opens again after the swap its own save makes;
  // and the field that had focus when that swap came, so typing carries on where it was.
  var opened = {};
  var focused = null;

  function canvasOf(element) {
    return element && element.closest ? element.closest("[data-doc-canvas]") : null;
  }

  function surfaceOf(canvas) {
    return canvas.querySelector("[data-doc-canvas-surface]");
  }

  function nodes(canvas) {
    return Array.prototype.slice.call(canvas.querySelectorAll("[data-doc-canvas-node]"));
  }

  function at(node) {
    return {
      x: parseFloat(node.style.getPropertyValue("--x") || node.getAttribute("data-x")) || 0,
      y: parseFloat(node.style.getPropertyValue("--y") || node.getAttribute("data-y")) || 0,
    };
  }

  function put(node, x, y) {
    node.style.setProperty("--x", Math.max(0, Math.round(x / SNAP) * SNAP));
    node.style.setProperty("--y", Math.max(0, Math.round(y / SNAP) * SNAP));
  }

  function size(node) {
    return {
      w: parseFloat(node.style.getPropertyValue("--w") || node.getAttribute("data-w")) || 0,
      h: parseFloat(node.style.getPropertyValue("--h") || node.getAttribute("data-h")) || 0,
    };
  }

  // Zero means the stylesheet decides, which is what everything starts at; anything else is in
  // the same grid steps as a position, with a floor so a thing cannot be resized into nothing.
  function sized(node, w, h) {
    node.style.setProperty("--w", Math.max(8, Math.round(w / SNAP) * SNAP));
    node.style.setProperty("--h", Math.max(4, Math.round(h / SNAP) * SNAP));
  }

  function frameOf(canvas) {
    return canvas.querySelector(".doc-canvas") || canvas;
  }

  function looking(canvas, x, y, zoom) {
    canvas.style.setProperty("--pan-x", Math.round(x));
    canvas.style.setProperty("--pan-y", Math.round(y));
    canvas.style.setProperty("--zoom", zoom);
  }

  function keep(canvas) {
    kept[keyOf(canvas)] = {
      x: parseFloat(canvas.style.getPropertyValue("--pan-x")) || 0,
      y: parseFloat(canvas.style.getPropertyValue("--pan-y")) || 0,
      zoom: zoomOf(canvas),
    };
  }

  function keyOf(canvas) {
    return canvas.getAttribute("data-doc-canvas-view") || canvas.getAttribute("data-doc-canvas-place") || "";
  }

  // A canvas drawn to be read rather than arranged, such as a view on a service's page: it pans
  // and zooms, and its things open into other views, but nothing on it moves.
  function still(canvas) {
    return canvas.hasAttribute("data-doc-canvas-still");
  }

  function nodeNamed(canvas, reference) {
    return nodes(canvas).filter(function (each) {
      return each.getAttribute("data-doc-canvas-node") === reference;
    })[0];
  }

  function menuNamed(canvas, reference) {
    return Array.prototype.filter.call(canvas.querySelectorAll("[data-doc-canvas-menu]"), function (each) {
      return each.getAttribute("data-doc-canvas-menu") === reference;
    })[0];
  }

  // Where the view opens: its primary thing in the middle of the window, zoomed out — never in —
  // until everything else on the view is in sight as well. Without a primary, the middle of
  // everything on it.
  function home(canvas) {
    var boxes = nodes(canvas).map(function (node) {
      var where = at(node);
      return {
        node: node,
        left: where.x * STEP,
        top: where.y * STEP,
        right: where.x * STEP + node.offsetWidth,
        bottom: where.y * STEP + node.offsetHeight,
      };
    });
    if (!boxes.length) {
      looking(canvas, 0, 0, 1);
      return;
    }
    var primary = canvas.getAttribute("data-doc-canvas-primary");
    var focus = boxes.filter(function (box) {
      return primary && box.node.getAttribute("data-doc-canvas-node") === primary;
    })[0];
    var around = focus || {
      left: Math.min.apply(null, boxes.map(function (box) { return box.left; })),
      top: Math.min.apply(null, boxes.map(function (box) { return box.top; })),
      right: Math.max.apply(null, boxes.map(function (box) { return box.right; })),
      bottom: Math.max.apply(null, boxes.map(function (box) { return box.bottom; })),
    };
    var middle = { x: (around.left + around.right) / 2, y: (around.top + around.bottom) / 2 };
    // How far the furthest thing reaches from the middle, each way, against half the window.
    var reach = { x: 1, y: 1 };
    boxes.forEach(function (box) {
      reach.x = Math.max(reach.x, middle.x - box.left, box.right - middle.x);
      reach.y = Math.max(reach.y, middle.y - box.top, box.bottom - middle.y);
    });
    var frame = frameOf(canvas);
    var zoom = Math.min(
      1,
      (frame.clientWidth / 2 - MARGIN) / reach.x,
      (frame.clientHeight / 2 - MARGIN) / reach.y
    );
    zoom = Math.max(LEAST, zoom);
    looking(canvas, frame.clientWidth / 2 - middle.x * zoom, frame.clientHeight / 2 - middle.y * zoom, zoom);
  }

  function say(canvas, text) {
    var region = canvas.querySelector("[data-doc-canvas-live]");
    if (region) region.textContent = text;
  }

  function named(node) {
    var name = node.querySelector(".doc-canvas__name");
    return name ? name.textContent.trim() : node.getAttribute("data-doc-canvas-node");
  }

  // Everything is positioned in grid steps, so a pointer's pixels become steps once, here.
  function zoomOf(canvas) {
    return parseFloat(canvas.style.getPropertyValue("--zoom")) || 1;
  }

  function steps(canvas, event) {
    var surface = surfaceOf(canvas);
    var box = surface.getBoundingClientRect();
    var zoom = zoomOf(canvas);
    return {
      x: (event.clientX - box.left) / (STEP * zoom),
      y: (event.clientY - box.top) / (STEP * zoom),
    };
  }

  function ask(canvas, url, values) {
    var target = canvas.getAttribute("data-doc-canvas-target") || canvasOf(canvas);
    if (!window.htmx || !url) return;
    // HTMX rather than fetch, so the CSRF header the body carries is sent and the answer swaps
    // the page the same way every other change does.
    window.htmx.ajax("POST", url, { values: values, target: target, swap: "outerHTML" });
  }

  function save(canvas, node, withSize) {
    var where = at(node);
    var values = {
      node: node.getAttribute("data-doc-canvas-node"),
      x: where.x,
      y: where.y,
    };
    // Only a resize sends a size, so moving something never flattens how big it was drawn.
    if (withSize) {
      var how = size(node);
      values.w = how.w;
      values.h = how.h;
    }
    ask(canvas, canvas.getAttribute("data-doc-canvas-place"), values);
  }

  function nearest(node, event) {
    var box = node.getBoundingClientRect();
    var gaps = {
      top: Math.abs(event.clientY - box.top),
      bottom: Math.abs(box.bottom - event.clientY),
      left: Math.abs(event.clientX - box.left),
      right: Math.abs(box.right - event.clientX),
    };
    return Object.keys(gaps).reduce(function (closest, side) {
      return gaps[side] < gaps[closest] ? side : closest;
    }, "top");
  }

  // Selecting a thing, which is what shows its × and its edit and expand buttons.
  function select(canvas, node) {
    canvas.querySelectorAll("[data-doc-canvas-node]").forEach(function (each) {
      var chosen = each === node;
      each.classList.toggle("doc-canvas__node--chosen", chosen);
      var off = each.querySelector("[data-doc-canvas-off]");
      if (off) off.hidden = !chosen;
      var tools = each.querySelector("[data-doc-canvas-tools]");
      if (tools) tools.hidden = !chosen;
    });
  }

  // Resizing to match what is around it: once a side comes within NEAR squares of another thing's,
  // it takes that measurement exactly, and keeps it until the pointer is dragged FREE squares past
  // — so matching two things is easy and getting away from a match is still possible.
  function matched(state, which, wanted) {
    var held = state.held[which];
    if (held !== null && Math.abs(wanted - held) > NEAR + FREE) {
      state.held[which] = null;
      held = null;
    }
    if (held !== null) return held;
    var closest = null;
    var gap = NEAR;
    state.others.forEach(function (other) {
      var apart = Math.abs(wanted - other[which]);
      if (apart <= gap) {
        gap = apart;
        closest = other[which];
      }
    });
    if (closest !== null) state.held[which] = closest;
    return closest === null ? wanted : closest;
  }

  function menus(canvas, open, where) {
    var key = keyOf(canvas);
    if (opened[key] && opened[key].which !== open) delete opened[key];
    canvas.querySelectorAll("[data-doc-canvas-menu]").forEach(function (menu) {
      var showing = menu.getAttribute("data-doc-canvas-menu") === open;
      menu.hidden = !showing;
      if (!showing || !where) return;
      // Opened where the line was clicked and kept whole: measured now it is shown, rather than
      // clamped against a guess at how big it is. Set from script because the page's CSP allows no
      // inline style attribute.
      var box = canvas.getBoundingClientRect();
      var wide = menu.offsetWidth || 320;
      var tall = menu.offsetHeight || 240;
      var at = {
        x: Math.max(8, Math.min(where.x - box.left, box.width - wide - 8)),
        y: Math.max(8, Math.min(where.y - box.top, box.height - tall - 8)),
      };
      menu.style.setProperty("--menu-x", at.x);
      menu.style.setProperty("--menu-y", at.y);
      opened[key] = { which: open, at: at };
    });
  }

  // A thing's panel goes beside it, never over it, so what each change does can be seen as it is
  // made: to its right where there is room, else its left, else underneath it.
  function beside(canvas, which) {
    menus(canvas, which);
    var node = nodeNamed(canvas, which);
    var menu = menuNamed(canvas, which);
    if (!node || !menu) return;
    opened[keyOf(canvas)] = { which: which, at: null };
    var area = canvas.getBoundingClientRect();
    var box = node.getBoundingClientRect();
    var wide = menu.offsetWidth;
    var tall = menu.offsetHeight;
    var gap = 12;
    var x = box.right - area.left + gap;
    var y = Math.max(8, Math.min(box.top - area.top, area.height - tall - 8));
    if (x + wide > area.width - 8) {
      x = box.left - area.left - gap - wide;
      if (x < 8) {
        x = Math.max(8, Math.min(box.left - area.left, area.width - wide - 8));
        y = box.bottom - area.top + gap;
      }
    }
    menu.style.setProperty("--menu-x", Math.round(x));
    menu.style.setProperty("--menu-y", Math.round(y));
  }

  // What is typed into a panel shows on its thing at once, so the box can be seen changing before
  // the field is left and the change saved.
  function preview(canvas, field) {
    var how = field.getAttribute("data-doc-canvas-preview");
    var menu = field.closest("[data-doc-canvas-menu]");
    var node = how && menu && nodeNamed(canvas, menu.getAttribute("data-doc-canvas-menu"));
    if (!node) return;
    var text = field.value.trim();
    if (how === "name") {
      var name = node.querySelector(".doc-canvas__name");
      if (name) name.textContent = text || field.getAttribute("data-doc-canvas-usual") || "";
    } else if (how === "description") {
      var note = node.querySelector(".doc-canvas__note");
      if (!note && text) {
        note = document.createElement("span");
        note.className = "doc-canvas__note";
        var what = node.querySelector(".doc-canvas__what");
        node.insertBefore(note, what ? what.nextSibling : node.firstChild);
      }
      if (note) {
        note.textContent = text;
        note.hidden = !text;
      }
    }
    redraw(canvas);
    beside(canvas, menu.getAttribute("data-doc-canvas-menu"));
  }

  // The lines, drawn from each node's middle. Recomputed whenever anything moves, because a line
  // that lags behind the thing it joins reads as a different diagram.
  // Where a line meets a thing: the middle of the side it was drawn from, or the middle of the
  // box when it names no side.
  function meets(box, side) {
    switch (side) {
      case "top":
        return { x: box.x + box.w / 2, y: box.y };
      case "bottom":
        return { x: box.x + box.w / 2, y: box.y + box.h };
      case "left":
        return { x: box.x, y: box.y + box.h / 2 };
      case "right":
        return { x: box.x + box.w, y: box.y + box.h / 2 };
      default:
        return { x: box.x + box.w / 2, y: box.y + box.h / 2 };
    }
  }

  // What each line says sits on it in a box of its own, so it reads over the line beneath. HTML
  // rather than SVG text, which can have no background; a layer above the lines and below the
  // things, made the first time it is wanted.
  function labelsOf(canvas, svg) {
    var labels = canvas.querySelector("[data-doc-canvas-labels]");
    if (labels) return labels;
    labels = document.createElement("div");
    labels.className = "doc-canvas__labels";
    labels.setAttribute("data-doc-canvas-labels", "");
    labels.setAttribute("aria-hidden", "true");
    svg.insertAdjacentElement("afterend", labels);
    return labels;
  }

  function redraw(canvas) {
    var svg = canvas.querySelector("[data-doc-canvas-lines]");
    if (!svg) return;
    var labels = labelsOf(canvas, svg);
    labels.textContent = "";
    var boxes = {};
    nodes(canvas).forEach(function (node) {
      var where = at(node);
      boxes[node.getAttribute("data-doc-canvas-node")] = {
        x: where.x,
        y: where.y,
        w: node.offsetWidth / STEP,
        h: node.offsetHeight / STEP,
      };
    });
    var drawn = "";
    canvas.querySelectorAll("[data-doc-canvas-line]").forEach(function (line) {
      var fromBox = boxes[line.getAttribute("data-from")];
      var toBox = boxes[line.getAttribute("data-to")];
      if (!fromBox || !toBox) return;
      var from = meets(fromBox, line.getAttribute("data-from-side"));
      var to = meets(toBox, line.getAttribute("data-to-side"));
      var claim = line.getAttribute("data-claim") || "";
      // A line that holds both ways has an arrow at its start as well, which the marker turns
      // round to face back.
      var start = line.getAttribute("data-both") === "true" ? ' marker-start="url(#doc-canvas-arrow)"' : "";
      drawn +=
        '<line class="doc-canvas__line" x1="' + from.x * STEP + '" y1="' + from.y * STEP +
        '" x2="' + to.x * STEP + '" y2="' + to.y * STEP + '" marker-end="url(#doc-canvas-arrow)"' +
        start + " />" +
        // A two-pixel line is hard to hit, so a wide transparent one over it takes the click.
        '<line class="doc-canvas__hit" data-doc-canvas-hit="' + claim + '" x1="' + from.x * STEP +
        '" y1="' + from.y * STEP + '" x2="' + to.x * STEP + '" y2="' + to.y * STEP + '" />';
      var label = line.getAttribute("data-label");
      if (label) {
        // It takes the click as the line does, so what a line says opens its menu too.
        var tag = document.createElement("span");
        tag.className = "doc-canvas__line-label";
        tag.setAttribute("data-doc-canvas-hit", claim);
        tag.textContent = label;
        tag.style.setProperty("--label-x", ((from.x + to.x) / 2) * STEP);
        tag.style.setProperty("--label-y", ((from.y + to.y) / 2) * STEP);
        labels.appendChild(tag);
      }
    });
    if (linking && linking.to) {
      drawn +=
        '<line class="doc-canvas__line doc-canvas__line--drawing" x1="' + linking.from.x * STEP +
        '" y1="' + linking.from.y * STEP + '" x2="' + linking.to.x * STEP +
        '" y2="' + linking.to.y * STEP + '" />';
    }
    svg.innerHTML =
      '<defs><marker id="doc-canvas-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6"' +
      ' markerHeight="6" orient="auto-start-reverse">' +
      '<path class="doc-canvas__arrow" d="M 0 0 L 10 5 L 0 10 z" /></marker></defs>' + drawn;
  }

  function setup(canvas) {
    if (canvas.hasAttribute("data-doc-canvas-ready")) return;
    canvas.setAttribute("data-doc-canvas-ready", "");
    // The attributes the page sent become the custom properties the stylesheet positions by.
    nodes(canvas).forEach(function (node) {
      var where = at(node);
      put(node, where.x, where.y);
      var how = size(node);
      if (how.w || how.h) sized(node, how.w, how.h);
    });
    var held = kept[keyOf(canvas)];
    if (held) looking(canvas, held.x, held.y, held.zoom);
    else home(canvas);
    keep(canvas);
    redraw(canvas);
    reopen(canvas);
  }

  // After a save has swapped the page, the panel that was open opens again beside its thing, with
  // focus back in the field it was in and anything typed there since kept.
  function reopen(canvas) {
    var open = opened[keyOf(canvas)];
    var menu = open && menuNamed(canvas, open.which);
    var node = open && nodeNamed(canvas, open.which);
    var was = focused;
    focused = null;
    if (!menu || (!open.at && !node)) {
      delete opened[keyOf(canvas)];
      return;
    }
    // A line's menu where it was opened; a thing's panel beside the thing, wherever that is now.
    if (open.at) {
      menus(canvas, open.which);
      menu.style.setProperty("--menu-x", open.at.x);
      menu.style.setProperty("--menu-y", open.at.y);
    } else {
      select(canvas, node);
      beside(canvas, open.which);
    }
    var field = was && document.getElementById(was.id);
    if (!field || !menu.contains(field)) return;
    if (was.typed && field.value !== was.value) {
      field.value = was.value;
      preview(canvas, field);
    }
    field.focus();
    if (was.typed && field.setSelectionRange) {
      try {
        field.setSelectionRange(was.start, was.end);
      } catch (ignored) {}
    }
  }

  function all(root) {
    (root || document).querySelectorAll("[data-doc-canvas]").forEach(setup);
  }

  // Carrying something from the palette is a pointer drag, like everything else here, rather than
  // the browser's own drag and drop, which froze the page on some desktops. A press that never
  // moves is left alone, so the item's Add button and its text work as they would anyway.
  //
  // The canvas under a point, if the point is on its open surface rather than a menu or the zoom.
  function landing(event) {
    var under = document.elementFromPoint(event.clientX, event.clientY);
    if (!under || !under.closest || !under.closest(".doc-canvas")) return null;
    if (under.closest("[data-doc-canvas-menu]") || under.closest("[data-doc-canvas-zoom]")) return null;
    return canvasOf(under);
  }

  function accepting(canvas) {
    document.querySelectorAll(".doc-canvas--accepting").forEach(function (each) {
      each.classList.remove("doc-canvas--accepting");
    });
    if (canvas) frameOf(canvas).classList.add("doc-canvas--accepting");
  }

  function putDown() {
    if (!carrying) return;
    if (carrying.ghost) carrying.ghost.remove();
    document.documentElement.classList.remove("doc-canvas-carrying");
    accepting(null);
    carrying = null;
  }

  document.addEventListener("pointerdown", function (event) {
    if (event.button !== 0 || !event.target || !event.target.closest) return;
    var item = event.target.closest("[data-doc-canvas-drag]");
    if (!item || event.target.closest("form, button")) return;
    carrying = {
      node: item.getAttribute("data-doc-canvas-drag"),
      item: item,
      from: { x: event.clientX, y: event.clientY },
      ghost: null,
    };
    event.preventDefault();
  });

  document.addEventListener("pointercancel", putDown);

  document.addEventListener("pointerdown", function (event) {
    if (!event.target || !event.target.closest) return;
    var canvas = canvasOf(event.target);
    if (!canvas) return;
    // Anything on the canvas that is itself interactive keeps its own behaviour: a line's menu, a
    // thing's ×, the zoom buttons. Starting a pan or a drag here would preventDefault and swallow
    // the click, which is why the × did nothing and a menu could not be used.
    if (
      event.target.closest("[data-doc-canvas-menu]") ||
      event.target.closest("[data-doc-canvas-off]") ||
      event.target.closest("[data-doc-canvas-tools]") ||
      event.target.closest("[data-doc-canvas-zoom]") ||
      event.target.closest("[data-doc-canvas-caption]")
    ) {
      return;
    }
    var handle = event.target.closest("[data-doc-canvas-handle]");
    var grip = event.target.closest("[data-doc-canvas-grip]");
    var node = event.target.closest("[data-doc-canvas-node]");
    var hit = event.target.closest("[data-doc-canvas-hit]");
    if (still(canvas)) {
      handle = grip = hit = null;
    }
    // A left press on a line is the start of a click, which opens its menu; it must not start a pan.
    if (hit && event.button === 0) {
      event.preventDefault();
      return;
    }
    if (grip && node && event.button === 0) {
      var now = size(node);
      sizing = {
        canvas: canvas,
        node: node,
        from: steps(canvas, event),
        was: { w: now.w || node.offsetWidth / STEP, h: now.h || node.offsetHeight / STEP },
        // What else is on the canvas, measured once: a size to match is one of these.
        others: nodes(canvas)
          .filter(function (each) {
            return each !== node;
          })
          .map(function (each) {
            return { w: each.offsetWidth / STEP, h: each.offsetHeight / STEP };
          }),
        held: { w: null, h: null },
        moved: false,
      };
      node.classList.add("doc-canvas__node--sizing");
      event.preventDefault();
      return;
    }
    // The right button pans, wherever it is pressed, and so does the left on the background — or
    // anywhere at all on a canvas that is only there to be read.
    if (event.button === 2 || event.button === 1 || (event.button === 0 && (!node || still(canvas)))) {
      // The pan is kept on the canvas, not the surface, because the grid is drawn there and has to
      // slide by exactly the same amount as the things standing on it.
      panning = {
        canvas: canvas,
        from: { x: event.clientX, y: event.clientY },
        pan: {
          x: parseFloat(canvas.style.getPropertyValue("--pan-x")) || 0,
          y: parseFloat(canvas.style.getPropertyValue("--pan-y")) || 0,
        },
      };
      canvas.classList.add("doc-canvas--panning");
      event.preventDefault();
      return;
    }
    if (handle && node) {
      var where = at(node);
      var box = {
        x: where.x,
        y: where.y,
        w: node.offsetWidth / STEP,
        h: node.offsetHeight / STEP,
      };
      var side = handle.getAttribute("data-doc-canvas-handle") || "";
      linking = {
        canvas: canvas,
        node: node,
        side: side,
        from: meets(box, side),
        to: null,
      };
      canvas.classList.add("doc-canvas--linking");
      event.preventDefault();
      return;
    }
    if (node && event.button === 0) {
      var start = at(node);
      var pointer = steps(canvas, event);
      dragging = {
        canvas: canvas,
        node: node,
        offset: { x: pointer.x - start.x, y: pointer.y - start.y },
        moved: false,
      };
      node.classList.add("doc-canvas__node--dragging");
      node.setPointerCapture && node.setPointerCapture(event.pointerId);
      event.preventDefault();
    }
  });

  document.addEventListener("pointermove", function (event) {
    if (carrying) {
      // Let go outside the window, where no release arrives: the next move with no button held
      // puts it down.
      if (!event.buttons) {
        putDown();
        return;
      }
      if (!carrying.ghost) {
        // Only a press that has moved a little is a drag.
        if (Math.abs(event.clientX - carrying.from.x) + Math.abs(event.clientY - carrying.from.y) < 4) {
          return;
        }
        var ghost = document.createElement("div");
        ghost.className = "doc-canvas__ghost";
        ghost.setAttribute("aria-hidden", "true");
        var label = carrying.item.firstElementChild;
        if (label) ghost.appendChild(label.cloneNode(true));
        document.body.appendChild(ghost);
        document.documentElement.classList.add("doc-canvas-carrying");
        carrying.ghost = ghost;
      }
      carrying.ghost.style.setProperty("--ghost-x", event.clientX);
      carrying.ghost.style.setProperty("--ghost-y", event.clientY);
      accepting(landing(event));
      return;
    }
    if (panning) {
      panning.canvas.style.setProperty("--pan-x", panning.pan.x + (event.clientX - panning.from.x));
      panning.canvas.style.setProperty("--pan-y", panning.pan.y + (event.clientY - panning.from.y));
      return;
    }
    if (sizing) {
      var now = steps(sizing.canvas, event);
      var wanted = {
        w: sizing.was.w + (now.x - sizing.from.x),
        h: sizing.was.h + (now.y - sizing.from.y),
      };
      sized(sizing.node, matched(sizing, "w", wanted.w), matched(sizing, "h", wanted.h));
      sizing.moved = true;
      redraw(sizing.canvas);
      return;
    }
    if (linking) {
      linking.to = steps(linking.canvas, event);
      var over = document.elementFromPoint(event.clientX, event.clientY);
      var onto = over && over.closest ? over.closest("[data-doc-canvas-node]") : null;
      linking.canvas.querySelectorAll(".doc-canvas__node--target").forEach(function (node) {
        node.classList.remove("doc-canvas__node--target");
      });
      if (onto && onto !== linking.node) onto.classList.add("doc-canvas__node--target");
      redraw(linking.canvas);
      return;
    }
    if (!dragging) return;
    var pointer = steps(dragging.canvas, event);
    put(dragging.node, pointer.x - dragging.offset.x, pointer.y - dragging.offset.y);
    dragging.moved = true;
    redraw(dragging.canvas);
  });

  document.addEventListener("pointerup", function (event) {
    if (carrying) {
      var dragged = Boolean(carrying.ghost);
      var node = carrying.node;
      putDown();
      var onto = dragged ? landing(event) : null;
      if (!onto) return;
      var where = steps(onto, event);
      say(onto, "Put it on the view.");
      ask(onto, onto.getAttribute("data-doc-canvas-add"), {
        node: node,
        x: Math.max(0, Math.round(where.x / SNAP) * SNAP),
        y: Math.max(0, Math.round(where.y / SNAP) * SNAP),
      });
      return;
    }
    if (panning) {
      panning.canvas.classList.remove("doc-canvas--panning");
      keep(panning.canvas);
      panning = null;
      return;
    }
    if (sizing) {
      var grown = sizing.node;
      var board = sizing.canvas;
      var changed = sizing.moved;
      grown.classList.remove("doc-canvas__node--sizing");
      sizing = null;
      if (changed) save(board, grown, true);
      return;
    }
    if (linking) {
      var over = document.elementFromPoint(event.clientX, event.clientY);
      var onto = over && over.closest ? over.closest("[data-doc-canvas-node]") : null;
      var joined = linking.canvas;
      var from = linking.node;
      var side = linking.side;
      joined.classList.remove("doc-canvas--linking");
      joined.querySelectorAll(".doc-canvas__node--target").forEach(function (node) {
        node.classList.remove("doc-canvas__node--target");
      });
      linking = null;
      redraw(joined);
      if (onto && onto !== from) {
        say(joined, named(from) + " now reaches " + named(onto) + ".");
        ask(joined, joined.getAttribute("data-doc-canvas-connect"), {
          from: from.getAttribute("data-doc-canvas-node"),
          to: onto.getAttribute("data-doc-canvas-node"),
          from_side: side,
          to_side: nearest(onto, event),
          relationship: joined.getAttribute("data-doc-canvas-relationship") || "calls",
        });
      }
      return;
    }
    if (!dragging) return;
    var moving = dragging.node;
    var board = dragging.canvas;
    var moved = dragging.moved;
    moving.classList.remove("doc-canvas__node--dragging");
    dragging = null;
    if (moved) save(board, moving, false);
  });

  // Without this the right button opens the browser's own menu halfway through a pan.
  document.addEventListener("contextmenu", function (event) {
    if (canvasOf(event.target)) event.preventDefault();
  });

  // A node can be moved from the keyboard as well, a step at a time, so arranging a map does not
  // need a pointer at all.
  document.addEventListener("keydown", function (event) {
    var node = event.target.closest && event.target.closest("[data-doc-canvas-node]");
    var canvas = canvasOf(node);
    if (!node || !canvas || still(canvas)) return;
    var by = { ArrowLeft: [-SNAP, 0], ArrowRight: [SNAP, 0], ArrowUp: [0, -SNAP], ArrowDown: [0, SNAP] };
    var move = by[event.key];
    if (!move) return;
    event.preventDefault();
    var where = at(node);
    put(node, where.x + move[0], where.y + move[1]);
    redraw(canvas);
    save(canvas, node, false);
  });

  // Zoom is the canvas's own property, read by the surface that scales and by the grid that has to
  // scale with it. Nothing about it is saved: how close somebody is looking is not part of the map.
  document.addEventListener("click", function (event) {
    if (!event.target || !event.target.closest) return;
    var button = event.target.closest("[data-doc-canvas-zoom]");
    var canvas = canvasOf(button);
    if (!button || !canvas) return;
    event.preventDefault();
    var how = button.getAttribute("data-doc-canvas-zoom");
    if (how === "reset") {
      home(canvas);
      keep(canvas);
      say(canvas, "Back to where the view opens.");
      return;
    }
    var zoom = zoomOf(canvas) * (how === "in" ? 1.2 : 1 / 1.2);
    zoom = Math.min(MOST, Math.max(LEAST, zoom));
    canvas.style.setProperty("--zoom", zoom);
    keep(canvas);
    say(canvas, "Zoom " + Math.round(zoom * 100) + " per cent.");
  });

  document.addEventListener("click", function (event) {
    if (!event.target || !event.target.closest) return;
    var canvas = canvasOf(event.target);
    if (!canvas) return;
    if (event.target.closest("[data-doc-canvas-menu-close]")) {
      menus(canvas, "");
      select(canvas, null);
      return;
    }
    // A click inside an open menu is the menu's own business.
    if (event.target.closest("[data-doc-canvas-menu]")) return;
    // A thing's edit button opens its panel beside it, with the name ready to change.
    var edit = event.target.closest("[data-doc-canvas-edit]");
    if (edit) {
      var which = edit.getAttribute("data-doc-canvas-edit");
      beside(canvas, which);
      var menu = menuNamed(canvas, which);
      var field = menu && menu.querySelector("input:not([type=hidden]), textarea, select");
      if (field) field.focus();
      return;
    }
    // Its expand button is a link, and goes where it says.
    if (event.target.closest("[data-doc-canvas-expand]")) return;
    var chosen = event.target.closest("[data-doc-canvas-node]");
    if (chosen) {
      if (!event.target.closest("[data-doc-canvas-off]")) select(canvas, chosen);
      menus(canvas, "");
      return;
    }
    select(canvas, null);
    // Clicking a line opens its menu, and clicking the canvas anywhere else puts an open menu away.
    // The menu opens on the click rather than the press: opened on the press it appears under the
    // pointer, the release lands on it, and the click goes to what the line and the menu share —
    // the canvas — which closed the menu again at once.
    var hit = event.target.closest("[data-doc-canvas-hit]");
    if (hit) {
      menus(canvas, hit.getAttribute("data-doc-canvas-hit"), { x: event.clientX, y: event.clientY });
    } else {
      menus(canvas, "");
    }
  });

  document.addEventListener("input", function (event) {
    var canvas = canvasOf(event.target);
    if (canvas && event.target.hasAttribute("data-doc-canvas-preview")) preview(canvas, event.target);
  });

  document.addEventListener("keydown", function (event) {
    var menu = event.target.closest && event.target.closest("[data-doc-canvas-menu]");
    var canvas = canvasOf(menu);
    if (!menu || !canvas) return;
    // Escape puts a menu away and goes back to what it was opened from.
    if (event.key === "Escape") {
      var which = menu.getAttribute("data-doc-canvas-menu");
      menus(canvas, "");
      var node = nodeNamed(canvas, which);
      if (node) node.focus();
      return;
    }
    // Enter in a one-line field saves it there and then rather than submitting the whole form,
    // which is what leaving the field would do anyway.
    if (event.key === "Enter" && event.target.matches("input[type=text], input:not([type])")) {
      event.preventDefault();
      event.target.dispatchEvent(new Event("change", { bubbles: true }));
    }
  });

  // The field that has focus as a save's answer arrives, so it can have it back afterwards.
  ["htmx:before:swap", "htmx:beforeSwap"].forEach(function (name) {
    document.addEventListener(name, function () {
      var active = document.activeElement;
      if (!active || !active.id || !active.closest || !active.closest("[data-doc-canvas-menu]")) {
        focused = null;
        return;
      }
      var typed = active.matches("input[type=text], input:not([type]), textarea");
      focused = {
        id: active.id,
        typed: typed,
        value: active.value,
        start: typed ? active.selectionStart : 0,
        end: typed ? active.selectionEnd : 0,
      };
    });
  });

  all();
  ["htmx:after:swap", "htmx:afterSwap"].forEach(function (name) {
    document.addEventListener(name, function () {
      all();
    });
  });
  window.addEventListener("resize", function () {
    document.querySelectorAll("[data-doc-canvas]").forEach(redraw);
  });
})();
