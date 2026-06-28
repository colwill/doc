// The link beside every page's heading (`[data-doc-permalink]`): a click copies the page's address
// to share, and says so, rather than opening the page again. A new-tab or other modified click is
// left to the browser, and without script it is an ordinary link to the page.
(function () {
  var toasts = document.querySelector(".doc-toasts");
  function toast(kind, title, text) {
    if (!toasts) return;
    var box = document.createElement("div");
    box.className = "doc-toast doc-toast--" + kind;
    var heading = document.createElement("p");
    heading.className = "doc-toast__title";
    heading.textContent = title;
    var content = document.createElement("div");
    content.className = "doc-toast__content";
    var line = document.createElement("p");
    line.textContent = text;
    content.appendChild(line);
    box.appendChild(heading);
    box.appendChild(content);
    toasts.appendChild(box);
    setTimeout(function () { box.remove(); }, 5000);
  }

  // The clipboard API is only there over HTTPS or on localhost; anywhere else the address is put
  // in a field out of sight and copied from there.
  function copy(text) {
    if (navigator.clipboard && window.isSecureContext) return navigator.clipboard.writeText(text);
    return new Promise(function (resolve, reject) {
      var field = document.createElement("textarea");
      field.value = text;
      field.setAttribute("readonly", "");
      field.style.position = "fixed";
      field.style.opacity = "0";
      document.body.appendChild(field);
      field.select();
      var copied = false;
      try { copied = document.execCommand("copy"); } catch (err) { copied = false; }
      field.remove();
      if (copied) resolve(); else reject();
    });
  }

  document.addEventListener("click", function (event) {
    var link = event.target.closest && event.target.closest("[data-doc-permalink]");
    if (!link || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    // The address bar as it stands, so a section scrolled to with `#` goes along too.
    copy(window.location.href).then(
      function () { toast("success", "Link copied", "Anyone signed in who is allowed to see this page can open it."); },
      function () { toast("error", "Link not copied", "Copy the address from your browser's address bar instead."); }
    );
  });
})();
