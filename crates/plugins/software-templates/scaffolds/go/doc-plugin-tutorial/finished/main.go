// The plugin as it is at the end of TUTORIAL.md: on a cloudy morning it tells someone something
// cheerful, and books their team a sunshine break. Compare your own with it, or run it instead:
// `go build -o bin/finished ./finished`, then run that as `make run` runs yours.
package main

import (
	"context"
	"embed"
	"html/template"
	"log/slog"
	"os"
	"strings"

	"{{ scaffold.module }}/internal/doc"
)

// Set by the build: `go build -ldflags "-X main.version=0.2.0"`. A version is tied to the binary
// it first registered with, so each change you deploy needs a new one.
var version = "0.1.0"

// The setting keys, each used where the manifest declares it and where it is read.
const (
	settingPerson    = "person"
	settingPlace     = "place"
	settingLatitude  = "latitude"
	settingLongitude = "longitude"
	settingCloudyAt  = "cloudy-at"
	settingTeam      = "team"
	settingMessages  = "messages"
	settingWhen      = "when"
)

// The pages are compiled into the binary, so the plugin is one file wherever it runs.
//
//go:embed pages
var pages embed.FS

var page = template.Must(template.ParseFS(pages, "pages/*.html"))

func main() {
	err := doc.Serve(doc.Plugin{
		Manifest: manifest(),
		Routes:   routes(),
		Run:      run,
		// Asked before anybody's settings are saved.
		CheckSettings: checkSettings,
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
		Settings: []doc.Setting{
			{Key: settingPerson, Label: "Who to cheer up", Kind: "text", Required: true, Group: "Who and where",
				Hint: "Their login in DOC, such as ada. They are the one told, and the one invited to the break."},
			{Key: settingPlace, Label: "Where they are", Kind: "text", Default: "London, UK", Group: "Who and where"},
			{Key: settingLatitude, Label: "Latitude", Kind: "number", Default: 51.5072, Group: "Who and where",
				Min: doc.Limit(-90), Max: doc.Limit(90)},
			{Key: settingLongitude, Label: "Longitude", Kind: "number", Default: -0.1276, Group: "Who and where",
				Min: doc.Limit(-180), Max: doc.Limit(180)},
			{Key: settingCloudyAt, Label: "Cloudy from", Kind: "number", Default: 70, Group: "The weather",
				Min: doc.Limit(0), Max: doc.Limit(100), Hint: "How much of the sky, in percent, is cloud before it counts as a cloudy day."},
			{Key: settingWhen, Label: "When to look at the sky", Kind: "cron", Default: "0 8 * * 1-5", Group: "The weather",
				Hint: "A cron expression, in UTC. Weekdays at 08:00 to start with."},
			{Key: settingTeam, Label: "Whose calendar gets the break", Kind: "text", Group: "The sunshine break",
				Hint: "A team, as team:payments-core. Leave it empty for no break."},
			{Key: settingMessages, Label: "What to say", Kind: "list", Group: "The sunshine break",
				Default: defaultMessages, Hint: "One a line; {place} is filled in. A different one each day."},
		},
		Schedules: []doc.Schedule{
			{Name: "morning", Cron: "0 8 * * 1-5", Setting: settingWhen,
				Description: "Looks at the sky, and cheers someone up if it is cloudy"},
		},
		Operations: []doc.Operation{
			{Name: "cheer", Label: "Look at the sky and cheer someone up", Method: "POST", Route: "cheers",
				Description: "Looks at the sky now, and on a cloudy day sends today's message and books the break."},
		},
		Data: &doc.Data{Collections: map[string]doc.Collection{
			"cheers": {
				Fields: map[string]doc.Field{
					"id":          {Type: "uuid", Key: true},
					"day":         {Type: "text", Required: true},
					"person":      {Type: "text", Required: true},
					"sky":         {Type: "text", Required: true},
					"cloud_cover": {Type: "number", Required: true},
					"message":     {Type: "text", Required: true},
					"event":       {Type: "text"},
					"by":          {Type: "text", Required: true},
				},
				Indexes: [][]string{
					{"day"},
				},
			},
		}},
	}
}

// checkSettings refuses settings that would not work, with a reason shown against the field.
func checkSettings(ctx context.Context, b *doc.Backend, proposed doc.Settings) doc.Verdict {
	problems := map[string]string{}
	if team := proposed.Text(settingTeam); team != "" && !strings.Contains(team, ":") {
		problems[settingTeam] = "Say what kind of thing it is first, as team:" + team + "."
	}
	if login := proposed.Text(settingPerson); login != "" {
		if _, err := findPerson(ctx, b, login); err != nil {
			problems[settingPerson] = err.Error()
		}
	}
	return doc.Verdict{Problems: problems}
}

var defaultMessages = []string{
	"It's grey over {place}, but you make the day brighter.",
	"Clouds over {place} today: the perfect excuse for a good cup of tea and a win or two.",
	"Even behind the clouds the sun is still shining. So are you!",
	"{place} is overcast, so here is some sunshine from us: you're doing great.",
}
