// Lighting up a graph: hovering or focusing a node colours the lines that reach it and the nodes
// at their far end, and fades the rest of the map back so the connection reads at a glance. Every
// node carries `data-node` and every edge `data-from`/`data-to`, so nothing here needs to know
// what a map is of — a service map, an access map and an automation's chain all light the same
// way. Whatever the map is of is already coloured by the stylesheet, without anyone reaching
// for it; that stays put until something else is lit.
(function () {
  var LIT = "doc-graph--lit";
  var EDGE = "doc-graph__edge--lit";
  var NODE = "doc-graph__node--lit";

  function graphOf(element) {
    return element && element.closest ? element.closest(".doc-graph") : null;
  }

  function unlight(graph) {
    graph.classList.remove(LIT);
    graph.removeAttribute("data-lit");
    graph.querySelectorAll("." + EDGE).forEach(function (edge) {
      edge.classList.remove(EDGE);
    });
    graph.querySelectorAll("." + NODE).forEach(function (node) {
      node.classList.remove(NODE);
    });
  }

  function light(graph, id) {
    var near = Object.create(null);
    near[id] = true;
    graph.querySelectorAll(".doc-graph__edge").forEach(function (edge) {
      var from = edge.getAttribute("data-from");
      var to = edge.getAttribute("data-to");
      if (from !== id && to !== id) return;
      edge.classList.add(EDGE);
      near[from === id ? to : from] = true;
    });
    graph.querySelectorAll("[data-node]").forEach(function (item) {
      if (!near[item.getAttribute("data-node")]) return;
      var shape = item.querySelector(".doc-graph__node");
      if (shape) shape.classList.add(NODE);
    });
    graph.setAttribute("data-lit", id);
    graph.classList.add(LIT);
  }

  // One node at a time, across every map on the page: whatever the pointer or the keyboard has
  // arrived at wins, and arriving anywhere else puts the maps back as they were. Nothing lit and
  // nothing arrived at is the common case, and costs no look at the page at all.
  var lit = [];

  function arrived(target) {
    var item = target && target.closest ? target.closest("[data-node]") : null;
    var graph = item ? graphOf(item) : null;
    if (!graph && !lit.length) return;
    var id = graph ? item.getAttribute("data-node") : null;
    lit = lit.filter(function (other) {
      if (other === graph && other.getAttribute("data-lit") === id) return true;
      unlight(other);
      return false;
    });
    if (graph && id && graph.getAttribute("data-lit") !== id) {
      light(graph, id);
      lit.push(graph);
    }
  }

  document.addEventListener("pointerover", function (event) {
    arrived(event.target);
  });

  document.addEventListener("focusin", function (event) {
    arrived(event.target);
  });

  // The pointer left the window rather than moving on to something else.
  document.addEventListener("pointerout", function (event) {
    if (event.relatedTarget) return;
    arrived(null);
  });
})();
