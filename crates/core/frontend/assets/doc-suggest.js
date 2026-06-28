// Suggestions while typing: inside x-data="docSuggest", a word starting with `/` in the x-ref="field"
// asks the x-ref="query" element for options through HTMX (the `doc-suggest` event), and choosing an
// option replaces the word with the option's data-value.
document.addEventListener("alpine:init", function () {
  window.Alpine.data("docSuggest", function () {
    return {
      open: false,
      start: -1,
      active: -1,
      lookup: function () {
        var field = this.$refs.field;
        var before = field.value.slice(0, field.selectionStart);
        var match = /(^|[\s([{])\/([\w.:~@+\/-]{2,})$/.exec(before);
        if (!match) {
          this.close();
          return;
        }
        this.start = before.length - match[2].length - 1;
        this.active = -1;
        this.$refs.query.value = match[2];
        this.$refs.query.dispatchEvent(new Event("doc-suggest"));
        this.open = true;
      },
      options: function () {
        return Array.prototype.slice.call(this.$refs.list.querySelectorAll("[data-value]"));
      },
      mark: function (step) {
        var options = this.options();
        if (!options.length) return;
        this.active = (this.active + step + options.length) % options.length;
        options.forEach(function (option, index) {
          option.classList.toggle("doc-suggest__option--active", index === this.active);
        }, this);
        options[this.active].scrollIntoView({ block: "nearest" });
      },
      navigate: function (event) {
        if (!this.open) return;
        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
          event.preventDefault();
          this.mark(event.key === "ArrowDown" ? 1 : -1);
        } else if ((event.key === "Enter" || event.key === "Tab") && this.active >= 0) {
          event.preventDefault();
          this.insert(this.options()[this.active].dataset.value);
        } else if (event.key === "Escape") {
          event.preventDefault();
          this.close();
        }
      },
      choose: function (event) {
        var option = event.target.closest("[data-value]");
        if (option) this.insert(option.dataset.value);
      },
      insert: function (value) {
        var field = this.$refs.field;
        var end = field.selectionStart;
        var text = value + " ";
        field.value = field.value.slice(0, this.start) + text + field.value.slice(end);
        field.selectionStart = field.selectionEnd = this.start + text.length;
        field.focus();
        this.close();
      },
      close: function () {
        this.open = false;
        this.active = -1;
      },
    };
  });
});
