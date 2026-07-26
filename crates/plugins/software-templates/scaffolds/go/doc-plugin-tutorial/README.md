# {{ values.title }}

{{ values.description }}

A DOC plugin, written in Go, created from DOC's **{{ template.title }}** template with its
tutorial. **Start with [TUTORIAL.md](TUTORIAL.md)**: it grows the plugin here, one step at a time,
from one that says hello into one that looks at the sky over London every morning and, on a cloudy
day, cheers a colleague up — telling them something in DOC, and booking their team a sunshine break
in DOC's calendar.

| | |
|---|---|
| Plugin ID | `{{ values.name }}` |
| In the navigation | **{{ values.title }}**, under **{{ values.menu }}** |
| Go module | `{{ scaffold.module }}` |
| Lifecycle | **{{ lifecycle.title }}.** {{ lifecycle.about }} |

## Getting it into DOC

DOC only accepts a plugin it has been told to expect. Whoever runs DOC:

1. adds `"{{ values.name }}"` to `ids` under `[plugins]` in DOC's `config/doc.toml`;
2. starts DOC again. Starting it makes the plugin's registration token and certificate in DOC's
   `secrets` directory.

Then, with Go 1.24 or newer:

```sh
make run SECRETS=path/to/doc/secrets
```

## What is here

| | |
|---|---|
| `TUTORIAL.md` | The tutorial |
| `main.go`, `routes.go`, `pages/` | The plugin, as the tutorial starts it |
| `finished/` | The plugin as it is at the end of the tutorial, to compare yours with |
| `internal/doc/` | DOC's plugin protocol. Nothing in it needs changing to write the plugin |

Once you have finished, delete `finished/` and make the plugin your own.
