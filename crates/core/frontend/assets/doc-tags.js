// Tags as badges: inside x-data="docTags", focusing or typing in the x-ref="field" asks the
// x-ref="query" element for options through HTMX (the `doc-suggest` event), and choosing one adds a
// badge to the x-ref="chosen" list, with a hidden input named by the root's data-name. data-max
// limits how many there may be; at the limit, a new choice replaces the last. Options already
// chosen are hidden. Backspace in an empty field removes the last badge.
document.addEventListener("alpine:init", function () {
  window.Alpine.data("docTags", function () {
    return {
      open: false,
      active: -1,
      init: function () {
        var self = this;
        new MutationObserver(function () {
          self.hideChosen();
        }).observe(this.$refs.list, { childList: true });
      },
      chosen: function () {
        return Array.prototype.slice.call(this.$refs.chosen.querySelectorAll("[data-value]"));
      },
      has: function (value) {
        return this.chosen().some(function (tag) {
          return tag.dataset.value === value;
        });
      },
      hideChosen: function () {
        this.$refs.list.querySelectorAll("[data-value]").forEach(function (option) {
          option.parentElement.hidden = this.has(option.dataset.value);
        }, this);
        this.active = -1;
      },
      lookup: function () {
        this.active = -1;
        this.$refs.query.value = this.$refs.field.value.trim();
        this.$refs.query.dispatchEvent(new Event("doc-suggest"));
        this.open = true;
      },
      options: function () {
        return Array.prototype.slice
          .call(this.$refs.list.querySelectorAll("[data-value]"))
          .filter(function (option) {
            return !option.parentElement.hidden;
          });
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
        if (event.key === "Backspace" && this.$refs.field.value === "") {
          var last = this.chosen().pop();
          if (last) {
            event.preventDefault();
            last.remove();
            this.hideChosen();
          }
          return;
        }
        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
          event.preventDefault();
          if (!this.open) this.lookup();
          this.mark(event.key === "ArrowDown" ? 1 : -1);
        } else if (event.key === "Enter" && this.open) {
          // Enter picks an option rather than sending the form, while there are options to pick.
          var options = this.options();
          if (!options.length) return;
          event.preventDefault();
          this.add(options[Math.max(this.active, 0)]);
        } else if (event.key === "Tab" && this.open && this.active >= 0) {
          event.preventDefault();
          this.add(this.options()[this.active]);
        } else if (event.key === "Escape" && this.open) {
          event.preventDefault();
          this.close();
        }
      },
      choose: function (event) {
        var option = event.target.closest("[data-value]");
        if (option) this.add(option);
      },
      add: function (option) {
        var value = option.dataset.value;
        var max = parseInt(this.$root.dataset.max || "0", 10);
        if (!this.has(value)) {
          var chosen = this.chosen();
          if (max > 0 && chosen.length >= max) chosen[chosen.length - 1].remove();
          this.$refs.chosen.appendChild(
            this.badge(value, option.dataset.label, option.dataset.hint, option.dataset.kind)
          );
        }
        this.$refs.field.value = "";
        this.$refs.field.focus();
        // Keep suggesting while more may be added; a single choice is done.
        if (max === 1) this.close();
        else this.lookup();
      },
      // An option with a data-kind is a catalogue resource: its badge is a doc-kind label in that
      // kind's colour, with the kind first. Anything else is a plain tag, the hint after the name.
      badge: function (value, label, hint, kind) {
        var tag = document.createElement("li");
        tag.dataset.value = value;
        var name = document.createElement("span");
        name.textContent = label || value;
        if (kind) {
          tag.className = "doc-kind doc-kind--" + kind;
          var plate = document.createElement("span");
          plate.className = "doc-kind__label";
          plate.textContent = hint || kind;
          tag.appendChild(plate);
          tag.appendChild(name);
        } else {
          tag.className = "doc-tags__tag";
          name.className = "doc-tags__label";
          tag.appendChild(name);
          if (hint) {
            var said = document.createElement("span");
            said.className = "doc-tags__kind";
            said.textContent = hint;
            tag.appendChild(said);
          }
        }
        var input = document.createElement("input");
        input.type = "hidden";
        input.name = this.$root.dataset.name || "tags";
        input.value = value;
        tag.appendChild(input);
        var remove = document.createElement("button");
        remove.type = "button";
        remove.className = "doc-tags__remove";
        remove.setAttribute("aria-label", "Remove " + (label || value));
        remove.textContent = "\u00d7";
        tag.appendChild(remove);
        return tag;
      },
      remove: function (event) {
        var button = event.target.closest(".doc-tags__remove");
        if (!button) return;
        button.closest("[data-value]").remove();
        this.hideChosen();
        this.$refs.field.focus();
      },
      close: function () {
        this.open = false;
        this.active = -1;
      },
    };
  });
});
