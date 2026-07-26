// {{ values.name }}, a DOC plugin. README.md says how to run it against DOC, and what to check
// before it is deployed anywhere that matters.
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
		Load:     load,
		Run:      run,
		Cancel:   cancel,
		OnEvent:  onEvent,
	})
	if err != nil {
		slog.Error("the plugin stopped", "error", err)
		os.Exit(1)
	}
}

const settingGreeting = "greeting"

// manifest is what the plugin tells DOC about itself when it starts. DOC builds its navigation
// entry, its Settings page, its collections, its schedules and what automations can call from it.
func manifest() doc.Manifest {
	return doc.Manifest{
		ID:             "{{ values.name }}",
		Version:        version,
		Classification: "{{ values.classification }}",
		Nav: []doc.Nav{
			{Label: {{ values.title | json }}, Path: "/", Description: {{ values.description | json }}, Group: "{{ values.menu }}"},
		},
		Settings: []doc.Setting{
			{Key: settingGreeting, Label: "Greeting", Kind: "text", Default: "Hello",
				Hint: "What the page greets people with. Replace it with the plugin's own settings."},
		},
		// Topic filters delivered to OnEvent, such as "plugin.github.pull-request.merged".
		Subscriptions: []string{},
		// Cron expressions, in UTC. Each has DOC run the plugin with {"schedule": "<name>"}:
		// {Name: "nightly", Cron: "0 2 * * *", Description: "Tidies up"}.
		Schedules: []doc.Schedule{},
		// What automations can call by name, and what anything reading the manifest can learn of
		// the plugin's API.
		Operations: []doc.Operation{
			{Name: "add-item", Label: "Add an item", Method: "POST", Route: "items",
				Description: "Adds an item to the plugin's list.",
				Params: []doc.OperationParam{
					{Name: "name", Label: "Name", Required: true},
				}},
		},
		// The plugin's collections. DOC creates them, and changes them when a new version declares
		// something new.
		Data: &doc.Data{Collections: map[string]doc.Collection{
			"items": {
				Fields: map[string]doc.Field{
					"id":   {Type: "uuid", Key: true},
					"name": {Type: "text", Required: true, Max: doc.Limit(200)},
					"by":   {Type: "text"},
				},
				Search: []string{"name"},
			},
		}},
	}
}
