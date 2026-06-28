// The browser's own drag and drop freezes the page on some desktops, so DOC starts it only for what
// it lets people drag on purpose — an element marked draggable="true", such as a row being arranged
// or a builder step. Dragging selected text, a link or an image does nothing instead.
(function () {
  document.addEventListener(
    "dragstart",
    function (event) {
      var source = event.target;
      if (source && source.nodeType === 1 && source.getAttribute("draggable") === "true") return;
      event.preventDefault();
    },
    true
  );
})();
