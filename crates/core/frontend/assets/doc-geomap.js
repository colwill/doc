// Maps (DOC-SPEC §11.24): places in the world drawn from what a page says in `data-geomap`, over
// Natural Earth's countries, which the platform keeps. A plugin sends coordinates or a country's
// code and the platform draws, so no plugin carries a map, a script or a tile server, and nothing
// is fetched from anywhere but DOC. Dragging pans; the buttons, the keys and Ctrl with the wheel
// zoom. The world is fetched the first time a page has a map, as Chart.js is.
(function () {
  var NS = "http://www.w3.org/2000/svg";
  var script = document.currentScript;
  var source = script && script.getAttribute("data-geometry");
  var TONES = ["ready", "degraded", "error", "unknown", "security"];
  var SHADES = 5;
  var MOST_ZOOM = 32;

  // Equal Earth (Šavrič, Patterson and Jenny, 2018): the whole world at once, every area true.
  var A1 = 1.340264, A2 = -0.081106, A3 = 0.000893, A4 = 0.003796, M = Math.sqrt(3) / 2;
  function raw(lon, lat) {
    var t = Math.asin(M * Math.sin((lat * Math.PI) / 180));
    var t2 = t * t, t6 = t2 * t2 * t2;
    var x = (2 * Math.sqrt(3) * ((lon * Math.PI) / 180) * Math.cos(t)) /
      (3 * (9 * A4 * t6 * t2 + 7 * A3 * t6 + 3 * A2 * t2 + A1));
    return [x, t * (A1 + A2 * t2 + t6 * (A3 + A4 * t2))];
  }
  var WIDTH = 1000;
  var SCALE = WIDTH / 2 / raw(180, 0)[0];
  var HEIGHT = 2 * raw(0, 90)[1] * SCALE;
  function project(lon, lat) {
    var at = raw(lon, lat);
    return [at[0] * SCALE, -at[1] * SCALE];
  }

  var world = null;
  var loading = false;
  var waiting = [];
  var drawn = [];

  function outline(rings) {
    var box = [Infinity, Infinity, -Infinity, -Infinity];
    var d = rings.map(function (flat) {
      var parts = [];
      for (var i = 0; i < flat.length; i += 2) {
        var at = project(flat[i], flat[i + 1]);
        box = [Math.min(box[0], at[0]), Math.min(box[1], at[1]), Math.max(box[2], at[0]), Math.max(box[3], at[1])];
        parts.push((i ? "L" : "M") + at[0].toFixed(1) + " " + at[1].toFixed(1));
      }
      return parts.join("") + "Z";
    }).join("");
    return { d: d, box: box };
  }

  // The world once per page: each country's outline projected, and found by its codes or names.
  function prepare(loaded) {
    var byCode = Object.create(null);
    var byName = Object.create(null);
    var countries = loaded.countries.map(function (country) {
      var drawnAs = outline(country.rings);
      var held = { names: country.names, d: drawnAs.d, box: drawnAs.box, label: project(country.label[0], country.label[1]) };
      if (country.a2) byCode[country.a2.toUpperCase()] = held;
      if (country.a3) byCode[country.a3.toUpperCase()] = held;
      country.names.forEach(function (name) { byName[name.toLowerCase()] = held; });
      return held;
    });
    var edge = [];
    for (var lat = -90; lat <= 90; lat += 5) edge.push(project(-180, lat));
    for (lat = 90; lat >= -90; lat -= 5) edge.push(project(180, lat));
    var sea = edge.map(function (at, i) { return (i ? "L" : "M") + at[0].toFixed(1) + " " + at[1].toFixed(1); }).join("") + "Z";
    return {
      countries: countries,
      sea: sea,
      find: function (named) {
        var text = String(named || "").trim();
        return byCode[text.toUpperCase()] || byName[text.toLowerCase()] || null;
      },
    };
  }

  function load(then) {
    if (world) return then();
    waiting.push(then);
    if (loading || !source) return;
    loading = true;
    fetch(source, { credentials: "same-origin" })
      .then(function (answer) { return answer.json(); })
      .then(function (loaded) {
        world = prepare(loaded);
        waiting.splice(0).forEach(function (go) { go(); });
      })
      .catch(function () { loading = false; });
  }

  function make(name, attributes, parent) {
    var element = document.createElementNS(NS, name);
    Object.keys(attributes || {}).forEach(function (key) { element.setAttribute(key, attributes[key]); });
    if (parent) parent.appendChild(element);
    return element;
  }

  function tone(value) {
    return TONES.indexOf(value) >= 0 ? value : "";
  }

  function said(place) {
    var label = place.label ? String(place.label) : "";
    var shown = place.shown != null ? String(place.shown) : place.value != null ? String(place.value) : "";
    return label && shown ? label + ": " + shown : label || shown;
  }

  // Where a place is, in the map's own units: its coordinates, or the label point of its country.
  // A place named only by its country is called what the country is.
  function placed(place) {
    if (place.country != null && place.lat == null) {
      var country = world.find(place.country);
      if (country && !place.label) place.label = country.names[0];
      return country ? country.label : null;
    }
    var lat = Number(place.lat), lon = Number(place.lon);
    if (!isFinite(lat) || !isFinite(lon) || Math.abs(lat) > 90 || Math.abs(lon) > 180) return null;
    return project(lon, lat);
  }

  function draw(figure) {
    var config;
    try {
      config = JSON.parse(figure.getAttribute("data-geomap") || "{}");
    } catch (ignored) {
      return;
    }
    figure.setAttribute("data-geomap-drawn", "");
    var missed = [];
    var svg = make("svg", {
      class: "doc-geomap__map",
      viewBox: [-WIDTH / 2, -HEIGHT / 2, WIDTH, HEIGHT].join(" "),
      role: "group",
      tabindex: "0",
      "aria-label": figure.getAttribute("aria-label") || "A map",
    });
    var view = make("g", { class: "doc-geomap__view" }, svg);
    make("path", { class: "doc-geomap__sea", d: world.sea }, view);

    // Countries a page colours: by tone, or shaded by value against the largest.
    var regions = new Map();
    var largestRegion = 0;
    (config.regions || []).forEach(function (region) {
      var country = world.find(region.country);
      if (!country) return missed.push(region.label || region.country);
      regions.set(country, region);
      if (!tone(region.tone) && isFinite(region.value)) largestRegion = Math.max(largestRegion, Math.abs(region.value));
    });
    world.countries.forEach(function (country) {
      var region = regions.get(country);
      var path = make("path", { class: "doc-geomap__land", d: country.d }, view);
      if (!region) return;
      var shade = tone(region.tone)
        ? "doc-geomap__region--" + tone(region.tone)
        : "doc-geomap__region--shade-" + (largestRegion ? Math.max(1, Math.ceil((SHADES * Math.abs(region.value || 0)) / largestRegion)) : SHADES);
      path.setAttribute("class", "doc-geomap__land doc-geomap__region " + shade);
      region.label = region.label || country.names[0];
      path.setAttribute("data-tip", said(region));
      make("title", {}, path).textContent = said(region);
      if (region.href) {
        var link = make("a", { href: region.href }, view);
        link.appendChild(path);
      }
    });

    // Points, the largest first so a small one is never hidden under it, sized by area.
    var points = [];
    var largest = 0;
    (config.points || []).forEach(function (point) {
      var at = placed(point);
      if (!at) return missed.push(point.label || point.country || [point.lat, point.lon].join(", "));
      points.push({ point: point, at: at });
      if (isFinite(point.value)) largest = Math.max(largest, Math.abs(point.value));
    });
    points.sort(function (one, two) { return Math.abs(two.point.value || 0) - Math.abs(one.point.value || 0); });
    var marks = points.map(function (held) {
      var point = held.point;
      var size = largest && isFinite(point.value) ? 4 + 12 * Math.sqrt(Math.abs(point.value) / largest) : 6;
      var parent = point.href ? make("a", { href: point.href }, view) : view;
      var circle = make("circle", {
        class: "doc-geomap__point" + (tone(point.tone) ? " doc-geomap__point--" + tone(point.tone) : ""),
        cx: held.at[0].toFixed(2),
        cy: held.at[1].toFixed(2),
        "data-tip": said(point),
      }, parent);
      if (!point.href) circle.setAttribute("tabindex", "0");
      make("title", {}, circle).textContent = said(point);
      return { circle: circle, size: size };
    });

    figure.appendChild(svg);
    var controls = document.createElement("div");
    controls.className = "doc-geomap__controls";
    [["+", "Zoom in"], ["−", "Zoom out"], ["Reset", "Show the map as it started"]].forEach(function (button) {
      var element = document.createElement("button");
      element.type = "button";
      element.className = "doc-geomap__control";
      element.textContent = button[0];
      element.setAttribute("aria-label", button[1]);
      controls.appendChild(element);
    });
    figure.appendChild(controls);
    var tip = document.createElement("div");
    tip.className = "doc-geomap__tip";
    tip.hidden = true;
    figure.appendChild(tip);
    if (missed.length) {
      var note = document.createElement("p");
      note.className = "doc-geomap__note";
      var named = missed.slice(0, 5).join(", ");
      note.textContent = "Not on the map, as DOC could not place " + (missed.length === 1 ? "it" : "them") + ": " + named +
        (missed.length > 5 ? " and " + (missed.length - 5) + " more" : "") + ".";
      figure.appendChild(note);
    }

    // The view: where the map's centre is and how far it is zoomed. Framing what is shown, where a
    // page asks, starts it on the places rather than the whole world.
    var start = { k: 1, x: 0, y: 0 };
    if (config.fit === "points") {
      var box = [Infinity, Infinity, -Infinity, -Infinity];
      points.forEach(function (held) {
        box = [Math.min(box[0], held.at[0]), Math.min(box[1], held.at[1]), Math.max(box[2], held.at[0]), Math.max(box[3], held.at[1])];
      });
      regions.forEach(function (region, country) {
        box = [Math.min(box[0], country.box[0]), Math.min(box[1], country.box[1]), Math.max(box[2], country.box[2]), Math.max(box[3], country.box[3])];
      });
      if (isFinite(box[0])) {
        var span = Math.max((box[2] - box[0]) * 1.5, ((box[3] - box[1]) * 1.5 * WIDTH) / HEIGHT, 40);
        start.k = Math.max(1, Math.min(MOST_ZOOM, WIDTH / span));
        start.x = -((box[0] + box[2]) / 2) * start.k;
        start.y = -((box[1] + box[3]) / 2) * start.k;
      }
    }
    var state = { k: start.k, x: start.x, y: start.y };

    function apply() {
      // The centre stays over the world, so the map cannot be dragged off and lost.
      var half = [(WIDTH / 2) * state.k, (HEIGHT / 2) * state.k];
      state.x = Math.max(-half[0], Math.min(half[0], state.x));
      state.y = Math.max(-half[1], Math.min(half[1], state.y));
      view.setAttribute("transform", "translate(" + state.x.toFixed(2) + " " + state.y.toFixed(2) + ") scale(" + state.k.toFixed(4) + ")");
      var width = svg.getBoundingClientRect().width || 640;
      var unit = WIDTH / width / state.k;
      marks.forEach(function (mark) { mark.circle.setAttribute("r", (mark.size * unit).toFixed(3)); });
    }

    function zoom(by, about) {
      var k = Math.max(1, Math.min(MOST_ZOOM, state.k * by));
      var at = about || [0, 0];
      var worldAt = [(at[0] - state.x) / state.k, (at[1] - state.y) / state.k];
      state.x = at[0] - worldAt[0] * k;
      state.y = at[1] - worldAt[1] * k;
      state.k = k;
      apply();
    }

    function local(event) {
      var matrix = svg.getScreenCTM();
      if (!matrix) return [0, 0];
      var at = svg.createSVGPoint();
      at.x = event.clientX;
      at.y = event.clientY;
      at = at.matrixTransform(matrix.inverse());
      return [at.x, at.y];
    }

    var buttons = controls.querySelectorAll("button");
    buttons[0].addEventListener("click", function () { zoom(2); });
    buttons[1].addEventListener("click", function () { zoom(0.5); });
    buttons[2].addEventListener("click", function () {
      state = { k: start.k, x: start.x, y: start.y };
      apply();
    });

    // The wheel scrolls the page as it always does; with Ctrl (or a pinch, which browsers send as
    // Ctrl) it zooms where the pointer is.
    svg.addEventListener("wheel", function (event) {
      if (!event.ctrlKey && !event.metaKey) return;
      event.preventDefault();
      zoom(Math.exp(-event.deltaY * 0.002), local(event));
    }, { passive: false });

    var dragging = null;
    var dragged = false;
    svg.addEventListener("pointerdown", function (event) {
      if (event.button !== 0) return;
      dragging = { from: local(event), x: state.x, y: state.y, id: event.pointerId };
      dragged = false;
    });
    svg.addEventListener("pointermove", function (event) {
      if (!dragging || event.pointerId !== dragging.id) return;
      var at = local(event);
      var moved = [at[0] - dragging.from[0], at[1] - dragging.from[1]];
      if (!dragged && Math.abs(moved[0]) + Math.abs(moved[1]) < 4) return;
      if (!dragged) {
        dragged = true;
        svg.setPointerCapture(event.pointerId);
        figure.classList.add("doc-geomap--dragging");
      }
      state.x = dragging.x + moved[0];
      state.y = dragging.y + moved[1];
      apply();
    });
    function stop() {
      dragging = null;
      figure.classList.remove("doc-geomap--dragging");
    }
    svg.addEventListener("pointerup", stop);
    svg.addEventListener("pointercancel", stop);
    // A drag that ends on a place does not follow its link.
    svg.addEventListener("click", function (event) {
      if (!dragged) return;
      event.preventDefault();
      dragged = false;
    }, true);

    svg.addEventListener("keydown", function (event) {
      if (event.target !== svg) return;
      var step = WIDTH / 10;
      var moves = { ArrowLeft: [step, 0], ArrowRight: [-step, 0], ArrowUp: [0, step], ArrowDown: [0, -step] };
      if (moves[event.key]) {
        state.x += moves[event.key][0];
        state.y += moves[event.key][1];
        apply();
      } else if (event.key === "+" || event.key === "=") {
        zoom(2);
      } else if (event.key === "-" || event.key === "_") {
        zoom(0.5);
      } else if (event.key === "0") {
        state = { k: start.k, x: start.x, y: start.y };
        apply();
      } else {
        return;
      }
      event.preventDefault();
    });

    // What a place is, beside it, while the pointer or the keyboard is on it.
    function show(event) {
      var target = event.target.closest ? event.target.closest("[data-tip]") : null;
      if (!target || !target.getAttribute("data-tip")) return;
      var at = target.getBoundingClientRect();
      var frame = figure.getBoundingClientRect();
      // Above the place, unless that would leave the map; then below it.
      var below = at.top - frame.top < 40;
      tip.textContent = target.getAttribute("data-tip");
      tip.classList.toggle("doc-geomap__tip--below", below);
      tip.style.left = at.left + at.width / 2 - frame.left + "px";
      tip.style.top = (below ? at.bottom : at.top) - frame.top + "px";
      tip.hidden = false;
    }
    function hide() {
      tip.hidden = true;
    }
    svg.addEventListener("pointerover", show);
    svg.addEventListener("focusin", show);
    svg.addEventListener("pointerout", hide);
    svg.addEventListener("focusout", hide);

    apply();
    drawn.push({ figure: figure, apply: apply });
  }

  function drawAll() {
    drawn = drawn.filter(function (held) { return document.body.contains(held.figure); });
    var waitingMaps = document.querySelectorAll("[data-geomap]:not([data-geomap-drawn])");
    if (!waitingMaps.length) return;
    load(function () {
      document.querySelectorAll("[data-geomap]:not([data-geomap-drawn])").forEach(draw);
    });
  }

  drawAll();
  new MutationObserver(drawAll).observe(document.body, { childList: true, subtree: true });
  window.addEventListener("resize", function () {
    drawn.forEach(function (held) { held.apply(); });
  });
})();
