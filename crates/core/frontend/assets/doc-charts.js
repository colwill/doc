// Draws every canvas that carries a Chart.js configuration in `data-chart`, including those HTMX
// swaps in later, so the page itself holds no script. Chart.js is fetched only once a page has a
// chart to draw, from where the layout's `data-chartjs` says it is.
(function () {
  var source = document.currentScript && document.currentScript.getAttribute("data-chartjs");
  var loading = false;
  var drawn = [];
  function draw() {
    // A chart whose canvas HTMX has swapped away is let go, so a panel that refreshes itself
    // does not keep every chart it ever drew.
    drawn = drawn.filter(function (canvas) {
      if (document.body.contains(canvas)) return true;
      canvas.chart.destroy();
      return false;
    });
    var canvases = document.querySelectorAll("canvas[data-chart]");
    if (!canvases.length) return;
    if (!window.Chart) {
      if (!loading && source) {
        loading = true;
        var script = document.createElement("script");
        script.src = source;
        script.onload = draw;
        document.head.appendChild(script);
      }
      return;
    }
    canvases.forEach(function (canvas) {
      if (canvas.chart) return;
      try {
        canvas.chart = new window.Chart(canvas, JSON.parse(canvas.getAttribute("data-chart")));
        drawn.push(canvas);
      } catch (ignored) {}
    });
  }
  draw();
  new MutationObserver(draw).observe(document.body, { childList: true, subtree: true });
})();
