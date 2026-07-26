// {{ values.name }}: the smallest DOC plugin there is. It says hello on a page of its own, and
// TUTORIAL.md grows it, one step at a time, into one that cheers somebody up on a cloudy day.
package main

import (
	"embed"
	"html/template"
	"log/slog"
	"os"

	"{{ scaffold.module }}/internal/doc"
)

// Set by the build: `go build -ldflags "-X main.version=0.2.0"`. A version is tied to the binary
// it first registered with, so each change you deploy needs a new one.
var version = "0.1.0"

// The pages are compiled into the binary, so the plugin is one file wherever it runs.
//
//go:embed pages
var pages embed.FS

var page = template.Must(template.ParseFS(pages, "pages/*.html"))

func main() {
	err := doc.Serve(doc.Plugin{
		Manifest: manifest(),
		Routes:   routes(),
	})
	if err != nil {
		slog.Error("the plugin stopped", "error", err)
		os.Exit(1)
	}
}

// manifest is what the plugin tells DOC about itself when it starts: its ID, and the link DOC
// puts in its navigation.
func manifest() doc.Manifest {
	return doc.Manifest{
		ID:             "{{ values.name }}",
		Version:        version,
		Classification: doc.Synchronous,
		Nav: []doc.Nav{
			{Label: {{ values.title | json }}, Path: "/", Description: {{ values.description | json }}, Group: "{{ values.menu }}"},
		},
	}
}
