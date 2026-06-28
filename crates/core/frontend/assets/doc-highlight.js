// Syntax for a `pre.doc-code`, marked up here so a plugin never has to. A block says what it
// holds with `data-doc-language="rust"`, or with `data-doc-filename="src/main.rs"` and lets the
// name say it. What comes back is the same text wrapped in `doc-code__*` spans, built as nodes
// rather than as HTML, so nothing a block holds can escape into the page.
//
// There is no grammar here, and there is deliberately not going to be one: comments, strings,
// numbers, a setting's name, a list of keywords and two things read off the shape of a name — a
// capital letter means a type, a bracket after it means it is being called — is what tells a file
// apart at a glance, and it is what survives being wrong. A language nobody listed is left alone.
(function () {
  var COMMON = "if else for while return break continue switch case default do try catch finally throw new delete typeof instanceof void null true false";

  // Each language says how it writes a comment and a string, and which words are its own.
  var LANGUAGES = {
    go: {
      symbols: true,
      line: ["//"],
      block: [["/*", "*/"]],
      quotes: ["\"", "'", "`"],
      keywords: "break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var nil true false iota make new len cap append copy delete panic recover string int int8 int16 int32 int64 uint uint8 uint16 uint32 uint64 float32 float64 bool byte rune error any",
    },
    rust: {
      symbols: true,
      line: ["//"],
      block: [["/*", "*/"]],
      quotes: ["\"", "'"],
      keywords: "as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while bool char str u8 u16 u32 u64 usize i8 i16 i32 i64 isize f32 f64",
    },
    python: {
      symbols: true,
      line: ["#"],
      block: [],
      quotes: ["\"\"\"", "'''", "\"", "'"],
      keywords: "and as assert async await break class continue def del elif else except finally for from global if import in is lambda None nonlocal not or pass raise return True False try while with yield self int str float bool list dict set tuple bytes",
    },
    cpp: {
      symbols: true,
      line: ["//"],
      block: [["/*", "*/"]],
      quotes: ["\"", "'"],
      keywords: COMMON + " auto bool char class const constexpr co_await co_return decltype double enum explicit export extern float friend inline int long mutable namespace noexcept nullptr operator override private protected public register reinterpret_cast short signed sizeof static static_cast struct template this thread_local throw union unsigned using virtual volatile wchar_t include define pragma ifndef endif",
    },
    typescript: {
      symbols: true,
      line: ["//"],
      block: [["/*", "*/"]],
      quotes: ["\"", "'", "`"],
      keywords: COMMON + " abstract any as async await boolean class const declare enum export extends from function get implements import in interface keyof let never number object of private protected public readonly set static string super this type undefined unknown var yield",
    },
    shell: {
      line: ["#"],
      block: [],
      quotes: ["\"", "'"],
      keywords: "if then elif else fi for while until do done case esac function return exit export local readonly set unset source echo cd test in",
    },
    dockerfile: {
      line: ["#"],
      block: [],
      quotes: ["\"", "'"],
      keywords: "FROM RUN CMD LABEL EXPOSE ENV ADD COPY ENTRYPOINT VOLUME USER WORKDIR ARG ONBUILD STOPSIGNAL HEALTHCHECK SHELL AS",
    },
    makefile: { line: ["#"], block: [], quotes: ["\"", "'"], keywords: ".PHONY include ifeq ifneq ifdef ifndef else endif define endef export", keys: true },
    yaml: { line: ["#"], block: [], quotes: ["\"", "'"], keywords: "true false null yes no on off", keys: true },
    toml: { line: ["#"], block: [], quotes: ["\"\"\"", "\"", "'"], keywords: "true false", keys: true },
    json: { line: [], block: [], quotes: ["\""], keywords: "true false null", keys: true },
    properties: { line: ["#"], block: [], quotes: ["\"", "'"], keywords: "true false", keys: true },
    proto: {
      symbols: true,
      line: ["//"],
      block: [["/*", "*/"]],
      quotes: ["\"", "'"],
      keywords: "syntax package import option message enum service rpc returns repeated optional required reserved oneof map stream extend bool string bytes int32 int64 uint32 uint64 float double true false",
    },
    sql: { line: ["--"], block: [["/*", "*/"]], quotes: ["'", "\""], keywords: "select from where insert into values update set delete create table alter drop index join left right inner outer on group by order having limit offset returning and or not null true false primary key foreign references default unique constraint as distinct union all with" },
  };

  // The same language under the names people write, and the file names that mean it.
  var ALIASES = {
    golang: "go", rs: "rust", py: "python", "c++": "cpp", cxx: "cpp", cc: "cpp", c: "cpp",
    h: "cpp", hpp: "cpp", ts: "typescript", tsx: "typescript", js: "typescript",
    javascript: "typescript", mjs: "typescript", jsx: "typescript", sh: "shell", bash: "shell",
    zsh: "shell", yml: "yaml", env: "properties", ini: "properties", conf: "properties",
    cfg: "properties", mk: "makefile", make: "makefile", docker: "dockerfile",
    postgres: "sql", psql: "sql",
  };

  var NAMED = {
    dockerfile: "dockerfile", containerfile: "dockerfile", makefile: "makefile",
    justfile: "makefile", "cargo.lock": "toml", "go.sum": "properties", "go.mod": "properties",
    ".env": "properties", ".env.example": "properties", ".gitignore": "properties",
  };

  function grammar(name) {
    if (!name) return null;
    var key = String(name).trim().toLowerCase();
    return LANGUAGES[key] || LANGUAGES[ALIASES[key]] || null;
  }

  // A file name says its language: by the whole name where that is what names it (a Dockerfile,
  // a Makefile), otherwise by what follows the last dot.
  function fromFilename(path) {
    if (!path) return null;
    var name = String(path).split("/").pop().toLowerCase();
    if (NAMED[name]) return LANGUAGES[NAMED[name]];
    var dot = name.lastIndexOf(".");
    return dot > -1 ? grammar(name.slice(dot + 1)) : null;
  }

  function words(list) {
    var set = Object.create(null);
    list.split(" ").forEach(function (word) {
      if (word) set[word] = true;
    });
    return set;
  }

  function starts(text, at, marker) {
    return text.substr(at, marker.length) === marker;
  }

  // One pass over the text. At each position the longest thing that starts here wins, and
  // anything that starts but never ends runs to the end of the block rather than being dropped.
  function scan(text, language) {
    var keywords = words(language.keywords || "");
    var pieces = [];
    var plain = "";
    var at = 0;

    function keep(kind, value) {
      if (plain) {
        pieces.push([null, plain]);
        plain = "";
      }
      pieces.push([kind, value]);
    }

    function until(from, marker) {
      var found = text.indexOf(marker, from);
      return found === -1 ? text.length : found + marker.length;
    }

    while (at < text.length) {
      var letter = text.charAt(at);
      var handled = false;

      for (var b = 0; b < (language.block || []).length && !handled; b += 1) {
        var pair = language.block[b];
        if (starts(text, at, pair[0])) {
          var blockEnd = until(at + pair[0].length, pair[1]);
          keep("comment", text.slice(at, blockEnd));
          at = blockEnd;
          handled = true;
        }
      }
      if (handled) continue;

      for (var l = 0; l < (language.line || []).length && !handled; l += 1) {
        if (starts(text, at, language.line[l]) && opensComment(text, at, language.line[l])) {
          var lineEnd = text.indexOf("\n", at);
          if (lineEnd === -1) lineEnd = text.length;
          keep("comment", text.slice(at, lineEnd));
          at = lineEnd;
          handled = true;
        }
      }
      if (handled) continue;

      for (var q = 0; q < (language.quotes || []).length && !handled; q += 1) {
        var quote = language.quotes[q];
        if (!starts(text, at, quote)) continue;
        var cursor = at + quote.length;
        while (cursor < text.length) {
          if (text.charAt(cursor) === "\\") {
            cursor += 2;
            continue;
          }
          if (starts(text, cursor, quote)) {
            cursor += quote.length;
            break;
          }
          // A one-line string stops at the end of its line, so an apostrophe in a sentence does
          // not paint the rest of the file green.
          if (quote.length === 1 && text.charAt(cursor) === "\n") break;
          cursor += 1;
        }
        keep("string", text.slice(at, Math.min(cursor, text.length)));
        at = Math.min(cursor, text.length);
        handled = true;
      }
      if (handled) continue;

      if (letter >= "0" && letter <= "9") {
        var number = /^[0-9][0-9a-fA-FxXoObB_.]*(?:[eE][+-]?[0-9]+)?/.exec(text.slice(at));
        if (number) {
          keep("number", number[0]);
          at += number[0].length;
          continue;
        }
      }

      // A language of settings reads `app.kubernetes.io/name` as one name, because that is one
      // name. A language of code reads a name a segment at a time, so the `Println` of
      // `fmt.Println` is what is being called and the `fmt` is not.
      var word = (language.keys ? /^[A-Za-z_$@.][A-Za-z0-9_$./-]*/ : /^[A-Za-z_$][A-Za-z0-9_$]*/)
        .exec(text.slice(at));
      if (word) {
        var value = word[0];
        var after = text.slice(at + value.length);
        if (keywords[value]) keep("keyword", value);
        else if (language.keys && /^[ \t]*[:=]/.test(after) && onlySpaceBefore(text, at)) {
          keep("key", value);
        } else if (language.symbols && /^!?\(/.test(after)) keep("function", value);
        else if (language.symbols && /^[A-Z]/.test(value)) keep("type", value);
        else plain += value;
        at += value.length;
        continue;
      }

      plain += letter;
      at += 1;
    }
    if (plain) pieces.push([null, plain]);
    return pieces;
  }

  // `#` starts a comment only at the start of a line or after a space, which is the rule YAML,
  // TOML, shell and env files keep — otherwise the `#z` of `https://x/y#z` would grey out a URL.
  // A marker of two characters (`//`, `--`) is a comment wherever it falls.
  function opensComment(text, at, marker) {
    if (marker !== "#" || at === 0) return true;
    return /\s/.test(text.charAt(at - 1));
  }

  // A name is a setting's name only at the start of its line, so `http://x` in a value is not one.
  function onlySpaceBefore(text, at) {
    for (var back = at - 1; back >= 0; back -= 1) {
      var letter = text.charAt(back);
      if (letter === "\n") return true;
      if (letter !== " " && letter !== "\t" && letter !== "-") return false;
    }
    return true;
  }

  function paint(block) {
    if (block.getAttribute("data-doc-highlighted") === "yes") return;
    var language =
      grammar(block.getAttribute("data-doc-language")) ||
      fromFilename(block.getAttribute("data-doc-filename"));
    block.setAttribute("data-doc-highlighted", "yes");
    if (!language) return;
    var target = block.querySelector("code") || block;
    var text = target.textContent;
    // Long enough and it is not worth reading a colour off anyway; leave it plain and fast.
    if (!text || text.length > 120000) return;
    var pieces = scan(text, language);
    var painted = document.createDocumentFragment();
    pieces.forEach(function (piece) {
      if (!piece[0]) {
        painted.appendChild(document.createTextNode(piece[1]));
        return;
      }
      var span = document.createElement("span");
      span.className = "doc-code__" + piece[0];
      span.textContent = piece[1];
      painted.appendChild(span);
    });
    target.textContent = "";
    target.appendChild(painted);
  }

  function paintAll(root) {
    if (!root || !root.querySelectorAll) return;
    root.querySelectorAll("pre.doc-code[data-doc-language], pre.doc-code[data-doc-filename]").forEach(paint);
  }

  document.addEventListener("DOMContentLoaded", function () {
    paintAll(document);
  });

  // A block that arrives with a fragment, or is revealed by opening a file in a tree, is painted
  // when it turns up rather than only at load.
  ["htmx:after:swap", "htmx:afterSwap"].forEach(function (name) {
    document.addEventListener(name, function (event) {
      paintAll(event.target || document);
    });
  });

  document.addEventListener("toggle", function (event) {
    if (event.target && event.target.tagName === "DETAILS" && event.target.open) {
      paintAll(event.target);
    }
  }, true);
})();
