// The page editor: a Markdown `<textarea>` inside `[data-doc-editor]` is edited as the formatted
// page, with a toolbar, and can be switched to its Markdown to edit that instead. The textarea
// stays the form's field: whatever is formatted is written back to it as Markdown before the
// form is sent. Its libraries are fetched only once a page has an editor, from where the layout's
// `data-*` attributes on this script say they are.
(function () {
  var script = document.currentScript;
  var sources = ["purify", "squire", "marked", "turndown", "gfm"].map(function (name) {
    return script && script.getAttribute("data-" + name);
  });
  var loaded = null;

  // The libraries, in order (Squire cleans with DOMPurify, Turndown's tables come from its GFM
  // plugin), fetched once however many editors ask.
  function load() {
    if (loaded) return loaded;
    loaded = Promise.all(sources.map(function (source) {
      return new Promise(function (resolve, reject) {
        if (!source) return reject(new Error("the editor's libraries are not configured"));
        var tag = document.createElement("script");
        tag.src = source;
        tag.async = false;
        tag.onload = resolve;
        tag.onerror = reject;
        document.head.appendChild(tag);
      });
    }));
    return loaded;
  }

  var BLOCKS = /^(P|DIV|H[1-6])$/;
  var STYLES = [["P", "Paragraph"], ["H1", "Heading 1"], ["H2", "Heading 2"], ["H3", "Heading 3"], ["H4", "Heading 4"]];

  function button(label, text, pressable) {
    var made = document.createElement("button");
    made.type = "button";
    made.className = "doc-editor__button";
    made.textContent = text || label;
    if (text) {
      made.setAttribute("aria-label", label);
      made.title = label;
    }
    if (pressable) made.setAttribute("aria-pressed", "false");
    return made;
  }

  function group(parent, label) {
    var made = document.createElement("div");
    made.className = "doc-editor__group";
    made.setAttribute("role", "group");
    made.setAttribute("aria-label", label);
    parent.appendChild(made);
    return made;
  }

  function markdownOf() {
    var service = new window.TurndownService({
      headingStyle: "atx",
      codeBlockStyle: "fenced",
      bulletListMarker: "-",
      emDelimiter: "*",
      strongDelimiter: "**",
      hr: "---",
    });
    service.use(window.turndownPluginGfm.gfm);
    // An image shown from where DOC keeps it is written back as the page wrote it.
    service.addRule("written-image", {
      filter: function (node) {
        return node.nodeName === "IMG" && node.hasAttribute("data-written");
      },
      replacement: function (content, node) {
        var title = node.getAttribute("title");
        return "![" + (node.getAttribute("alt") || "") + "](" + node.getAttribute("data-written") +
          (title ? ' "' + title.replace(/"/g, '\\"') + '"' : "") + ")";
      },
    });
    // The line break an editor keeps at the end of a block so it can be typed in is not the
    // writer's.
    service.addRule("trailing-break", {
      filter: function (node) {
        return node.nodeName === "BR" && !node.nextSibling;
      },
      replacement: function () { return ""; },
    });
    return function (html) { return service.turndown(html).trim() + "\n"; };
  }

  // Markdown as the formatted page, its images shown from wherever DOC keeps them.
  function htmlOf(markdown, images) {
    var holder = document.createElement("template");
    holder.innerHTML = window.marked.parse(markdown, { gfm: true });
    holder.content.querySelectorAll("img").forEach(function (img) {
      var written = img.getAttribute("src");
      if (written && images[written]) {
        img.setAttribute("data-written", written);
        img.setAttribute("src", images[written]);
      }
    });
    return holder.innerHTML;
  }

  function enhance(container) {
    var source = container.querySelector("textarea");
    if (!source || container.classList.contains("doc-editor--ready")) return;
    container.classList.add("doc-editor--ready");
    var images = {};
    try { images = JSON.parse(source.getAttribute("data-doc-editor-images") || "{}"); } catch (ignored) {}
    var label = container.querySelector("label[for='" + source.id + "']");

    var toolbar = document.createElement("div");
    toolbar.className = "doc-editor__toolbar";
    toolbar.setAttribute("role", "toolbar");
    toolbar.setAttribute("aria-label", "Formatting");
    var page = document.createElement("div");
    page.className = "doc-editor__page doc-prose";
    page.setAttribute("role", "textbox");
    page.setAttribute("aria-multiline", "true");
    if (label) {
      label.id = label.id || source.id + "-label";
      page.setAttribute("aria-labelledby", label.id);
    }
    var linking = document.createElement("div");
    linking.className = "doc-editor__link";
    linking.hidden = true;
    source.parentNode.insertBefore(toolbar, source);
    source.parentNode.insertBefore(linking, source);
    source.parentNode.insertBefore(page, source);

    var styles = document.createElement("select");
    styles.className = "doc-select doc-select--small doc-editor__style";
    styles.setAttribute("aria-label", "Text style");
    STYLES.forEach(function (style) {
      var option = document.createElement("option");
      option.value = style[0];
      option.textContent = style[1];
      styles.appendChild(option);
    });
    var text = group(toolbar, "Text");
    text.appendChild(styles);
    var bold = text.appendChild(button("Bold", "B", true));
    var italic = text.appendChild(button("Italic", "I", true));
    var struck = text.appendChild(button("Strikethrough", "S", true));
    var code = text.appendChild(button("Code", null, true));
    var blocks = group(toolbar, "Blocks");
    var bullets = blocks.appendChild(button("Bulleted list", null, true));
    var numbers = blocks.appendChild(button("Numbered list", null, true));
    var quote = blocks.appendChild(button("Quote", null, true));
    var link = blocks.appendChild(button("Link", null, true));
    var history = group(toolbar, "History");
    var undo = history.appendChild(button("Undo"));
    var redo = history.appendChild(button("Redo"));
    var views = group(toolbar, "View");
    views.classList.add("doc-editor__views");
    var formattedView = views.appendChild(button("Formatted", null, true));
    var markdownView = views.appendChild(button("Markdown", null, true));
    bold.classList.add("doc-editor__button--bold");
    italic.classList.add("doc-editor__button--italic");
    struck.classList.add("doc-editor__button--struck");

    var linkLabel = document.createElement("label");
    linkLabel.className = "doc-label";
    linkLabel.textContent = "Link to";
    linkLabel.htmlFor = source.id + "-link";
    var linkField = document.createElement("input");
    linkField.className = "doc-input";
    linkField.id = source.id + "-link";
    linkField.type = "text";
    linkField.placeholder = "https://… or another-page.md";
    var linkAdd = button("Add link");
    var linkCancel = button("Cancel");
    var linkRow = document.createElement("div");
    linkRow.className = "doc-field-row";
    linkRow.appendChild(linkField);
    linkRow.appendChild(linkAdd);
    linkRow.appendChild(linkCancel);
    linking.appendChild(linkLabel);
    linking.appendChild(linkRow);

    var editor;
    var toMarkdown;
    var formatted = true;
    var changed = false;
    var kept = null;

    function setView(asFormatted) {
      if (asFormatted && !formatted) editor.setHTML(htmlOf(source.value, images));
      if (!asFormatted && formatted) source.value = toMarkdown(editor.getHTML());
      formatted = asFormatted;
      page.hidden = !formatted;
      source.hidden = formatted;
      linking.hidden = true;
      formattedView.setAttribute("aria-pressed", String(formatted));
      markdownView.setAttribute("aria-pressed", String(!formatted));
      [styles, bold, italic, struck, code, bullets, numbers, quote, link, undo, redo].forEach(function (control) {
        control.disabled = !formatted;
      });
      (formatted ? editor : source).focus();
    }

    function setBlock(tag) {
      editor.modifyBlocks(function (fragment) {
        Array.prototype.slice.call(fragment.childNodes).forEach(function (node) {
          if (!BLOCKS.test(node.nodeName) || node.nodeName === tag) return;
          var block = document.createElement(tag);
          while (node.firstChild) block.appendChild(node.firstChild);
          fragment.replaceChild(block, node);
        });
        return fragment;
      });
    }

    function show(path) {
      var within = function (tag) { return new RegExp("(^|>)" + tag + "(\\b|>|$)").test(path); };
      var heading = path.match(/(^|>)(H[1-4])\b/);
      styles.value = heading ? heading[2] : "P";
      bold.setAttribute("aria-pressed", String(within("B") || within("STRONG")));
      italic.setAttribute("aria-pressed", String(within("I") || within("EM")));
      struck.setAttribute("aria-pressed", String(within("S") || within("DEL")));
      code.setAttribute("aria-pressed", String(within("CODE") || within("PRE")));
      bullets.setAttribute("aria-pressed", String(within("UL")));
      numbers.setAttribute("aria-pressed", String(within("OL")));
      quote.setAttribute("aria-pressed", String(within("BLOCKQUOTE")));
      link.setAttribute("aria-pressed", String(within("A")));
    }

    function toggle(control, on, off) {
      return function () {
        if (control.getAttribute("aria-pressed") === "true") off(); else on();
        editor.focus();
      };
    }

    load().then(function () {
      toMarkdown = markdownOf();
      editor = new window.Squire(page, { blockTag: "P", blockAttributes: null });
      var start = container.querySelector("template[data-doc-editor-html]");
      if (start && !source.value.trim()) {
        editor.setHTML(start.innerHTML);
        source.value = toMarkdown(editor.getHTML());
      } else {
        editor.setHTML(htmlOf(source.value, images));
      }
      editor.addEventListener("pathChange", function (event) { show(event.detail.path || ""); });
      editor.addEventListener("input", function () { changed = true; });
      editor.addEventListener("undoStateChange", function (event) {
        undo.disabled = !event.detail.canUndo;
        redo.disabled = !event.detail.canRedo;
      });
      source.addEventListener("input", function () { changed = true; });
      styles.addEventListener("change", function () { setBlock(styles.value); editor.focus(); });
      bold.addEventListener("click", toggle(bold, function () { editor.bold(); }, function () { editor.removeBold(); }));
      italic.addEventListener("click", toggle(italic, function () { editor.italic(); }, function () { editor.removeItalic(); }));
      struck.addEventListener("click", toggle(struck, function () { editor.strikethrough(); }, function () { editor.removeStrikethrough(); }));
      code.addEventListener("click", function () { editor.toggleCode(); editor.focus(); });
      bullets.addEventListener("click", toggle(bullets, function () { editor.makeUnorderedList(); }, function () { editor.removeList(); }));
      numbers.addEventListener("click", toggle(numbers, function () { editor.makeOrderedList(); }, function () { editor.removeList(); }));
      quote.addEventListener("click", toggle(quote, function () { editor.increaseQuoteLevel(); }, function () { editor.removeQuote(); }));
      undo.addEventListener("click", function () { editor.undo(); editor.focus(); });
      redo.addEventListener("click", function () { editor.redo(); editor.focus(); });
      link.addEventListener("click", function () {
        if (link.getAttribute("aria-pressed") === "true") {
          editor.removeLink();
          editor.focus();
          return;
        }
        // The selection is kept while the field has the focus, and given back to add the link.
        kept = editor.getSelection();
        linking.hidden = false;
        linkField.value = "";
        linkField.focus();
      });
      function closeLink() {
        linking.hidden = true;
        if (kept) editor.setSelection(kept);
        editor.focus();
      }
      linkAdd.addEventListener("click", function () {
        var url = linkField.value.trim();
        closeLink();
        if (url) editor.makeLink(url);
      });
      linkCancel.addEventListener("click", closeLink);
      linkField.addEventListener("keydown", function (event) {
        if (event.key === "Enter") { event.preventDefault(); linkAdd.click(); }
        if (event.key === "Escape") { event.preventDefault(); closeLink(); }
      });
      formattedView.addEventListener("click", function () { setView(true); });
      markdownView.addEventListener("click", function () { setView(false); });
      undo.disabled = redo.disabled = true;
      setView(true);
      container.docEditor = {
        // Writes the formatted page into the field, as the form is about to send it.
        sent: function () {
          if (formatted) source.value = toMarkdown(editor.getHTML());
          changed = false;
        },
        changed: function () { return changed; },
      };
    }, function () {
      // Without its libraries the field is still a Markdown field, which is what it was.
      toolbar.remove();
      linking.remove();
      page.remove();
      container.classList.remove("doc-editor--ready");
      container.classList.add("doc-editor--plain");
    });
    page.hidden = true;
  }

  function enhanceAll() {
    document.querySelectorAll("[data-doc-editor]").forEach(enhance);
  }

  // Before HTMX or the browser reads the form: capturing, so it runs ahead of either.
  document.addEventListener("submit", function (event) {
    event.target.querySelectorAll("[data-doc-editor]").forEach(function (container) {
      if (container.docEditor) container.docEditor.sent();
    });
  }, true);

  window.addEventListener("beforeunload", function (event) {
    var unsaved = Array.prototype.some.call(document.querySelectorAll("[data-doc-editor]"), function (container) {
      return container.docEditor && container.docEditor.changed();
    });
    if (unsaved) {
      event.preventDefault();
      event.returnValue = "";
    }
  });

  enhanceAll();
  new MutationObserver(enhanceAll).observe(document.body, { childList: true, subtree: true });
})();
