# Writing a DOC plugin in Go

This repository holds the smallest DOC plugin there is: it registers with DOC, and shows a page
that says hello. By the end of this tutorial it will do something worth having. Every weekday
morning it will look at the sky over London, and on a cloudy day it will:

- tell a colleague something cheerful, in their DOC inbox — **another person**;
- book their team a fifteen-minute sunshine break, in DOC's calendar — **another plugin**;
- having asked a weather service what the sky is doing — **something outside DOC**.

It keeps a record of what it sent, has a page and an API of its own, a Settings page DOC draws for
it, and a button that does the same thing on demand. Each step ends with something to check, and
[`finished/`](finished) is the plugin as it is at the end, to compare yours with.

It takes about an hour. You need to know a little Go; nothing about DOC.

1. [Before you start](#1-before-you-start)
2. [Run the plugin](#2-run-the-plugin)
3. [Give it settings](#3-give-it-settings)
4. [Ask the weather](#4-ask-the-weather)
5. [Cheer somebody up](#5-cheer-somebody-up)
6. [Book a sunshine break](#6-book-a-sunshine-break)
7. [Remember what was sent](#7-remember-what-was-sent)
8. [Every cloudy morning](#8-every-cloudy-morning)
9. [Refuse settings that would not work](#9-refuse-settings-that-would-not-work)
10. [Check your work](#10-check-your-work)
11. [Where next](#11-where-next)

## 1. Before you start

You need:

- **Go 1.24 or newer.** `go version` says which you have.
- **A DOC to run it against**, with the `notifications`, `calendar` and `calendar-events` plugins
  running. A checkout of DOC started with `just dev` has all of them.
- **The plugin's ID, `{{ values.name }}`, known to that DOC.** A plugin cannot register with a DOC
  that has not been told to expect it. Whoever runs it — you, for `just dev` — adds the ID to `ids`
  under `[plugins]` in DOC's `config/doc.toml`:

  ```toml
  ids = [
      "{{ values.name }}",
      "rbac",
      # …
  ]
  ```

  and starts DOC again. Starting it makes the plugin's credentials — a registration token and a
  certificate named after it — in DOC's `secrets` directory.

A plugin is a program of its own that runs beside DOC and talks to it over HTTP/3. DOC calls it to
load it, to serve its pages and to run it; it calls DOC to read and write its data, to read its
settings and to reach other plugins. `internal/doc` speaks that protocol, so you will not need to
open it. The rest of this repository is the plugin:

| | |
|---|---|
| `main.go` | Starts the plugin, and says what it is: its **manifest** |
| `routes.go` | Its pages and, later, its API |
| `pages/` | The HTML for its pages, compiled into the binary |
| `internal/doc/` | DOC's plugin protocol |
| `finished/` | The plugin as it is at the end of this tutorial |

## 2. Run the plugin

Build it and start it, saying where DOC's secrets are:

```sh
make run SECRETS=path/to/doc/secrets
```

It prints `registered with the backend … state=loading`, and DOC's own log says it is `running`.
Leave it running, and open DOC.

**Check:** the **{{ values.menu }}** menu has **{{ values.title }}** in it, and it opens a page that
says hello to you by name.

What happened: `doc.Serve` sent DOC the manifest from `main.go`. DOC put the `Nav` entry in its
navigation, and when you opened it, forwarded the request to the plugin as `GET /ui/`. `routes.go`
routes that to `home`, which renders `pages/home.html`. DOC wraps what it answers in the platform's
layout — the header, the menus, the styles — which is why the page is a fragment: it uses only
DOC's `doc-*` classes, and has no `<script>` or `<style>` of its own.

`doc.From(r)` is DOC acting for that one request. `Caller()` says who is asking; DOC has already
checked they may read the plugin, which is why the page does not.

To stop it, press Ctrl-C. Each time you change the code from here on, stop it and `make run` again.

## 3. Give it settings

The plugin needs to know who to cheer up, and where they are. DOC draws a Settings page for any
plugin that declares settings, checks what is typed into it, stores it, and gives it to the plugin
when asked. Declare them in `main.go`.

Add the keys, under `version`:

```go
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
```

and the settings themselves, in `manifest()`, after `Nav`:

```go
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
```

The last one's default is a list of cheerful things to say. Add it at the end of `main.go`:

```go
var defaultMessages = []string{
	"It's grey over {place}, but you make the day brighter.",
	"Clouds over {place} today: the perfect excuse for a good cup of tea and a win or two.",
	"Even behind the clouds the sun is still shining. So are you!",
	"{place} is overcast, so here is some sunshine from us: you're doing great.",
}
```

**Check:** run it again. **Admin → Plugins** has a **Settings** link for {{ values.name }}, and its
page has every field, in its groups, with the defaults filled in. Try a latitude of 100: DOC refuses
it without asking the plugin. Put your own login in **Who to cheer up**, and save.

## 4. Ask the weather

Now something from outside DOC. [Open-Meteo](https://open-meteo.com) says what the weather is
anywhere, with no account or key. Create `weather.go`:

```go
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"strconv"
	"time"
)

// The weather comes from Open-Meteo, which needs no account and no key.
const forecast = "https://api.open-meteo.com/v1/forecast"

var client = &http.Client{Timeout: 10 * time.Second}

// Sky is the weather where somebody is, now.
type Sky struct {
	// Their local time, as Open-Meteo gives it: 2026-09-27T08:00.
	Time string
	// Their time zone, such as Europe/London.
	Zone        string
	CloudCover  float64
	Code        int
	Temperature float64
}

// lookUp asks Open-Meteo what the sky is doing at a place.
func lookUp(ctx context.Context, latitude, longitude float64) (Sky, error) {
	query := url.Values{
		"latitude":  {strconv.FormatFloat(latitude, 'f', 4, 64)},
		"longitude": {strconv.FormatFloat(longitude, 'f', 4, 64)},
		"current":   {"cloud_cover,weather_code,temperature_2m"},
		"timezone":  {"auto"},
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, forecast+"?"+query.Encode(), nil)
	if err != nil {
		return Sky{}, err
	}
	resp, err := client.Do(req)
	if err != nil {
		return Sky{}, fmt.Errorf("the weather could not be asked for: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return Sky{}, fmt.Errorf("the weather service answered %s", resp.Status)
	}
	var answer struct {
		Timezone string `json:"timezone"`
		Current  struct {
			Time        string  `json:"time"`
			CloudCover  float64 `json:"cloud_cover"`
			WeatherCode int     `json:"weather_code"`
			Temperature float64 `json:"temperature_2m"`
		} `json:"current"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&answer); err != nil {
		return Sky{}, fmt.Errorf("the weather could not be read: %w", err)
	}
	return Sky{
		Time:        answer.Current.Time,
		Zone:        answer.Timezone,
		CloudCover:  answer.Current.CloudCover,
		Code:        answer.Current.WeatherCode,
		Temperature: answer.Current.Temperature,
	}, nil
}

// Cloudy is whether at least this much of the sky, in percent, is cloud.
func (s Sky) Cloudy(from float64) bool { return s.CloudCover >= from }

// Day is the date where they are, which is what "once a day" means.
func (s Sky) Day() string {
	if len(s.Time) >= 10 {
		return s.Time[:10]
	}
	return time.Now().UTC().Format(time.DateOnly)
}

// Local is their time now, as a clock on their wall shows it.
func (s Sky) Local() time.Time {
	at, err := time.Parse("2006-01-02T15:04", s.Time)
	if err != nil {
		return time.Now().UTC()
	}
	return at
}

// String is the sky in words: "overcast, 100% cloud, 14°C".
func (s Sky) String() string {
	return fmt.Sprintf("%s, %.0f%% cloud, %.0f°C", described(s.Code), s.CloudCover, s.Temperature)
}

// described is a WMO weather code in words.
func described(code int) string {
	switch {
	case code == 0:
		return "clear"
	case code == 1:
		return "mainly clear"
	case code == 2:
		return "partly cloudy"
	case code == 3:
		return "overcast"
	case code == 45 || code == 48:
		return "foggy"
	case code >= 51 && code <= 57:
		return "drizzle"
	case code >= 61 && code <= 67, code >= 80 && code <= 82:
		return "rain"
	case code >= 71 && code <= 77, code == 85 || code == 86:
		return "snow"
	case code >= 95:
		return "thunder"
	}
	return "unsettled"
}
```

Nothing in it is about DOC: a plugin reaches the rest of the world as any Go program does.

Show the sky on the page. Replace `Home` and `home` in `routes.go` with:

```go
// Home is what the page shows.
type Home struct {
	ID      string
	Title   string
	Name    string
	Person  string
	Place   string
	Sky     string
	Cloudy  bool
	Writes  bool
	Said    string
	Problem string
}

func home(w http.ResponseWriter, r *http.Request) {
	show(w, r, "", "")
}

// show draws the whole page, with what the last action said or what went wrong.
func show(w http.ResponseWriter, r *http.Request, said, problem string) {
	ctx, b := r.Context(), doc.From(r)
	view := Home{
		ID:      b.ID(),
		Title:   manifest().Nav[0].Label,
		Name:    b.Caller().Name(),
		Writes:  b.Caller().Writes(),
		Said:    said,
		Problem: problem,
	}
	settings, err := b.Settings(ctx)
	if err != nil {
		view.Problem = err.Error()
	} else {
		view.Person, view.Place = settings.Text(settingPerson), settings.Text(settingPlace)
		sky, err := lookUp(ctx, settings.Number(settingLatitude, 51.5072), settings.Number(settingLongitude, -0.1276))
		if err != nil && view.Problem == "" {
			view.Problem = err.Error()
		}
		if err == nil {
			view.Sky, view.Cloudy = sky.String(), sky.Cloudy(settings.Number(settingCloudyAt, 70))
		}
	}
	w.Header().Set("content-type", "text/html; charset=utf-8")
	if err := page.ExecuteTemplate(w, "home.html", view); err != nil {
		slog.Error("the page could not be drawn", "error", err)
	}
}
```

`b.Settings` reads the settings as they stand: what was saved, or the default. Read them where they
are used rather than keeping a copy, and a change on the Settings page applies straight away.

Then replace `pages/home.html` with:

{% raw %}```html
{{/* The page at /p/<id>/. DOC wraps it in the platform's layout, so it is a fragment: only
     doc-* classes, no <script>, no <style> and no style="", and html/template escapes every value. */}}
{{define "home.html"}}
<div id="cheer">
  <h2>{{.Title}}</h2>
  <p class="doc-hint">Hello, {{.Name}}. On a cloudy morning this tells {{with .Person}}{{.}}{{else}}somebody{{end}} something cheerful, and books their team a sunshine break.</p>
  {{with .Problem}}<p class="doc-error-message" role="alert">{{.}}</p>{{end}}
  <div class="doc-card">
    <h3 class="doc-card__heading">The sky over {{.Place}}</h3>
    <div class="doc-card__content">
      {{if .Sky}}
      <p><strong class="doc-badge doc-badge--{{if .Cloudy}}degraded{{else}}ready{{end}}">{{if .Cloudy}}Cloudy{{else}}Bright{{end}}</strong> {{.Sky}}</p>
      {{else}}
      <p>The weather could not be read just now.</p>
      {{end}}
      {{if not .Person}}<p>Choose who to cheer up on the <a href="/plugins/{{.ID}}/settings">Settings page</a>.</p>{{end}}
    </div>
  </div>
</div>
{{end}}
```{% endraw %}

**Check:** the page shows the sky over London, such as **Cloudy** overcast, 100% cloud, 14°C. Change
the latitude and longitude on the Settings page to somewhere sunnier, and the page follows.

## 5. Cheer somebody up

Now another person. Two things are needed: who they are in DOC, and a way to tell them.

Everyone who has signed in to DOC is in `core.users`, which any plugin may read and none may
change. DOC's `notifications` plugin keeps each person's inbox — the bell in the header — and any
plugin may put something in it. Create `person.go`:

```go
package main

import (
	"context"
	"fmt"

	"{{ scaffold.module }}/internal/doc"
)

// Person is somebody in DOC.
type Person struct {
	ID    string
	Login string
}

// findPerson looks somebody up by their login. Every plugin may read core.users; none may change it.
func findPerson(ctx context.Context, b *doc.Backend, login string) (Person, error) {
	found, _, err := b.Find(ctx, doc.Query{
		Collection: "core.users",
		Where:      map[string]any{"login": login},
		Limit:      1,
	})
	if err != nil {
		return Person{}, fmt.Errorf("DOC could not be asked who %s is: %w", login, err)
	}
	if len(found) == 0 {
		return Person{}, fmt.Errorf("nobody in DOC signs in as %s", login)
	}
	return Person{ID: found[0].Text("id"), Login: login}, nil
}

// tell puts a notification in their DOC inbox: the bell in the header, and /p/notifications/.
func tell(ctx context.Context, b *doc.Backend, person Person, title, message, link string) error {
	return b.Notify(ctx, person.ID, title, message, link)
}
```

The notification comes from the plugin: DOC says so, whatever the plugin claims.

What happens on a cloudy day goes in one place, so the button now and the schedule later do the
same thing. Create `cheer.go`:

```go
package main

import (
	"context"
	"fmt"
	"strings"

	"{{ scaffold.module }}/internal/doc"
)

// Outcome is what looking at the sky came to.
type Outcome struct {
	Sky     string `json:"sky"`
	Cloudy  bool   `json:"cloudy"`
	Cheered bool   `json:"cheered"`
	Said    string `json:"said"`
	Event   string `json:"event,omitempty"`
}

// cheer looks at the sky where the person is and, on a cloudy day, tells them something cheerful
// and books their team a sunshine break.
func cheer(ctx context.Context, b *doc.Backend) (Outcome, error) {
	settings, err := b.Settings(ctx)
	if err != nil {
		return Outcome{}, err
	}
	login := settings.Text(settingPerson)
	if login == "" {
		return Outcome{}, fmt.Errorf("nobody has been chosen to cheer up yet: set it on the Settings page")
	}
	place := settings.Text(settingPlace)
	sky, err := lookUp(ctx, settings.Number(settingLatitude, 51.5072), settings.Number(settingLongitude, -0.1276))
	if err != nil {
		return Outcome{}, err
	}
	outcome := Outcome{Sky: sky.String(), Cloudy: sky.Cloudy(settings.Number(settingCloudyAt, 70))}
	if !outcome.Cloudy {
		outcome.Said = fmt.Sprintf("It is %s over %s: no cheering needed today.", sky, place)
		return outcome, nil
	}

	person, err := findPerson(ctx, b, login)
	if err != nil {
		return Outcome{}, err
	}
	message := todays(settings.List(settingMessages), sky, place)

	link := outcome.Event
	if link == "" {
		link = "/p/" + b.ID() + "/"
	}
	if err := tell(ctx, b, person, "A little sunshine for a cloudy day", message, link); err != nil {
		return Outcome{}, err
	}

	outcome.Cheered = true
	outcome.Said = fmt.Sprintf("Told %s: %q", person.Login, message)
	return outcome, nil
}

// todays is one of the messages, a different one each day, with the place filled in.
func todays(messages []string, sky Sky, place string) string {
	if len(messages) == 0 {
		messages = defaultMessages
	}
	day := sky.Local().YearDay()
	return strings.ReplaceAll(messages[day%len(messages)], "{place}", place)
}
```

Give the page a button. In `routes.go`, route the form to it, after the two `GET` lines:

```go
	mux.HandleFunc("POST /ui/cheers", cheerFromPage)
```

and add what it does, at the end:

```go
// cheerFromPage is the button: the page is drawn again with what happened.
func cheerFromPage(w http.ResponseWriter, r *http.Request) {
	outcome, err := cheer(r.Context(), doc.From(r))
	if err != nil {
		show(w, r, "", err.Error())
		return
	}
	show(w, r, outcome.Said, "")
}
```

In `pages/home.html`, show what it said, under the `Problem` line:

{% raw %}```html
  {{with .Said}}<p class="doc-hint" role="status">{{.}}</p>{{end}}
```{% endraw %}

and add the form after the card's closing `</div>`:

{% raw %}```html
  {{if .Writes}}
  <form hx-post="/p/{{.ID}}/cheers" hx-target="#cheer" hx-swap="outerHTML">
    <div class="doc-button-group">
      <button class="doc-button" type="submit">Look at the sky and cheer them up</button>
    </div>
  </form>
  {{end}}
```{% endraw %}

A page changes things only with `hx-post` and its kind. DOC sends it with the page's CSRF token,
checks that whoever pressed it may write to the plugin, and forwards it as `POST /ui/cheers`; the
page drawn again replaces `#cheer`. Someone who may only read the plugin is not shown the button,
and DOC would refuse them if they sent it anyway.

**Check:** press the button. On a cloudy day the page says **Told** you something, and the bell in
DOC's header has it, from {{ values.name }}. On a clear day, set **Cloudy from** to 0 first.

## 6. Book a sunshine break

Now another plugin. Plugins reach each other through DOC, never directly: `b.Peer` calls another
plugin's `discovery/` routes as this plugin, and that plugin decides whom it serves. `calendar-events`
lets any plugin put an event on a resource's calendar — a team's, say — and change only the events
it made itself. Create `calendar.go`:

```go
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"time"

	"{{ scaffold.module }}/internal/doc"
)

// The wall-clock form the calendar takes a time in, read in the event's own time zone.
const wallClock = "2006-01-02T15:04:05"

// bookBreak puts a fifteen-minute sunshine break on a team's calendar, an hour from now where they
// are, with the person invited. It asks the calendar-events plugin through its discovery/ routes, as
// this plugin: a plugin may put events on any resource's calendar, and change only its own.
func bookBreak(ctx context.Context, b *doc.Backend, team string, person Person, sky Sky) (string, error) {
	start := sky.Local().Truncate(time.Hour).Add(time.Hour)
	event := map[string]any{
		"on":          team,
		"title":       "Sunshine break",
		"description": fmt.Sprintf("It is %s. Fifteen minutes away from the screen, on us.", sky),
		"timezone":    sky.Zone,
		"start":       start.Format(wallClock),
		"end":         start.Add(15 * time.Minute).Format(wallClock),
		"attendees": []map[string]string{
			{"user": person.Login},
		},
	}
	status, body, err := b.Peer(ctx, "calendar-events", http.MethodPost, "events", event)
	if err != nil {
		return "", fmt.Errorf("the calendar could not be asked: %w", err)
	}
	if status != http.StatusCreated {
		return "", fmt.Errorf("the calendar answered %d: %s", status, body)
	}
	var made struct {
		ID string `json:"id"`
	}
	if err := json.Unmarshal(body, &made); err != nil {
		return "", err
	}
	return "/p/calendar/events/" + made.ID, nil
}
```

In `cheer.go`, book it before telling them, so the notification can link to it. Put this above
`link := outcome.Event`:

```go
	// The break is a kindness, not the point: a calendar that is not there does not stop the message.
	if team := settings.Text(settingTeam); team != "" {
		outcome.Event, err = bookBreak(ctx, b, team, person, sky)
		if err != nil {
			slog.Warn("the sunshine break could not be booked", "error", err)
		}
	}
```

and add `"log/slog"` to its imports.

**Check:** set **Whose calendar gets the break** to one of your teams, as `team:` and its name, and
press the button. The notification now opens the **Sunshine break** on that team's calendar, an hour
from now, with you invited.

## 7. Remember what was sent

A plugin keeps its data in collections it declares in its manifest; DOC creates them, and changes
them when a new version declares something new. Declare one for the cheers sent, at the end of
`manifest()`, after `Settings`:

```go
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
```

The key, `id`, is given one when a record is added without it. `day` is indexed, because the next
step looks cheers up by it.

Record who asked for each one. In `cheer.go`, `cheer` now takes that:

```go
func cheer(ctx context.Context, b *doc.Backend, by string) (Outcome, error) {
```

and in `routes.go`, `cheerFromPage` says it was the person who pressed the button:

```go
	outcome, err := cheer(r.Context(), doc.From(r), doc.From(r).Caller().Name())
```

Then keep each cheer, and tell the page. In `cheer.go`, after the `tell` block and before
`outcome.Cheered = true`:

```go
	_, err = b.Insert(ctx, "cheers", map[string]any{
		"day":         sky.Day(),
		"person":      person.Login,
		"sky":         sky.String(),
		"cloud_cover": sky.CloudCover,
		"message":     message,
		"event":       outcome.Event,
		"by":          by,
	})
	if err != nil {
		return Outcome{}, err
	}
	// Every open copy of the page is listening for this, and loads itself again.
	_ = b.Publish(ctx, "ui.cheers", map[string]string{"person": person.Login})
```

Show them on the page. In `routes.go`, `Home` gets a field for them, after `Problem`:

```go
	Cheers  []doc.Record
```

`show` reads the newest twenty, just before it sets the content type:

```go
	view.Cheers, _, err = b.Find(ctx, doc.Query{Collection: "cheers", Order: doc.Newest, Limit: 20})
	if err != nil && view.Problem == "" {
		view.Problem = err.Error()
	}
```

and the table has a route of its own, so it can be loaded again without the rest of the page. Add
it after the `GET` lines:

```go
	mux.HandleFunc("GET /ui/cheers", cheersTable)
```

and, above `cheerFromPage`:

```go
// cheersTable is the table on its own, which the page loads again when a cheer is sent.
func cheersTable(w http.ResponseWriter, r *http.Request) {
	b := doc.From(r)
	view := Home{ID: b.ID()}
	view.Cheers, _, _ = b.Find(r.Context(), doc.Query{Collection: "cheers", Order: doc.Newest, Limit: 20})
	w.Header().Set("content-type", "text/html; charset=utf-8")
	if err := page.ExecuteTemplate(w, "cheers", view); err != nil {
		slog.Error("the table could not be drawn", "error", err)
	}
}
```

In `pages/home.html`, put the table in after the form's `{{end}}`:

{% raw %}```html
  {{template "cheers" .}}
```{% endraw %}

and add it at the end of the file:

{% raw %}```html
{{/* The cheers sent so far. It loads itself again whenever the plugin publishes ui.cheers. */}}
{{define "cheers"}}
<div id="cheers" hx-get="/p/{{.ID}}/cheers" hx-trigger="doc-plugin-ui-{{.ID}} from:body" hx-swap="outerHTML">
  <h3>Sent so far</h3>
  {{if .Cheers}}
  <table class="doc-table">
    <thead class="doc-table__head">
      <tr class="doc-table__row">
        <th class="doc-table__header" scope="col">Day</th>
        <th class="doc-table__header" scope="col">To</th>
        <th class="doc-table__header" scope="col">The sky</th>
        <th class="doc-table__header" scope="col">What they were told</th>
        <th class="doc-table__header" scope="col">Asked by</th>
      </tr>
    </thead>
    <tbody class="doc-table__body">
      {{range .Cheers}}
      <tr class="doc-table__row">
        <td class="doc-table__cell">{{.day}}</td>
        <td class="doc-table__cell">{{.person}}</td>
        <td class="doc-table__cell">{{.sky}}</td>
        <td class="doc-table__cell">{{.message}}{{with .event}} <a href="{{.}}">The break</a>{{end}}</td>
        <td class="doc-table__cell">{{.by}}</td>
      </tr>
      {{end}}
    </tbody>
  </table>
  {{else}}
  <p class="doc-hint">Nothing yet: the sky has been kind, or nobody has looked.</p>
  {{end}}
</div>
{{end}}
```{% endraw %}

Publishing `ui.cheers` is what keeps every open copy of the page up to date: DOC turns an event
under `plugin.{{ values.name }}.ui.` into `doc-plugin-ui-{{ values.name }}` in each browser showing
one, and the table asks for itself again.

**Check:** press the button twice. Both cheers are in the table. Open the page in a second window
and press it in the first: the second window's table catches up by itself.

## 8. Every cloudy morning

Nobody should have to press a button. A **schedule** in the manifest has DOC run the plugin
whenever its cron expression comes round; `Setting` lets **When to look at the sky** change it
without a new version. In `manifest()`, between `Settings` and `Data`:

```go
		Schedules: []doc.Schedule{
			{Name: "morning", Cron: "0 8 * * 1-5", Setting: settingWhen,
				Description: "Looks at the sky, and cheers someone up if it is cloudy"},
		},
		Operations: []doc.Operation{
			{Name: "cheer", Label: "Look at the sky and cheer someone up", Method: "POST", Route: "cheers",
				Description: "Looks at the sky now, and on a cloudy day sends today's message and books the break."},
		},
```

The **operation** says, to anything that reads the manifest, that `POST api/cheers` does the same
as the button. DOC's automations offer it by name — **{{ values.name }} · Look at the sky and cheer
someone up** — so somebody can cheer a colleague up whenever a release goes out, without reading
this code.

A schedule might run twice, and a morning should bring one cheer however often the plugin looks.
In `cheer.go`, `cheer` takes one more thing:

```go
func cheer(ctx context.Context, b *doc.Backend, by string, oncePerDay bool) (Outcome, error) {
```

and, straight after the `if !outcome.Cloudy { … }` block, looks for one already sent today:

```go
	if oncePerDay {
		already, _, err := b.Find(ctx, doc.Query{Collection: "cheers", Where: map[string]any{"day": sky.Day()}, Limit: 1})
		if err != nil {
			return Outcome{}, err
		}
		if len(already) > 0 {
			outcome.Said = "Already cheered up today."
			return outcome, nil
		}
	}
```

What the schedule runs goes at the end of `cheer.go`:

```go
// run is what the schedule starts each morning. Nobody is waiting on it, so it looks only once a
// day however often it is run.
func run(ctx context.Context, b *doc.Backend, started doc.Run) (any, error) {
	outcome, err := cheer(ctx, b, "the morning", true)
	if err != nil {
		return nil, err
	}
	slog.Info("looked at the sky", "schedule", started.Schedule(), "sky", outcome.Sky, "cheered", outcome.Cheered)
	return outcome, nil
}
```

and `main` hands it to DOC, after `Routes`:

```go
		Run:      run,
```

A person pressing the button means it, so in `routes.go` the button asks for a cheer whatever
has been sent today:

```go
	outcome, err := cheer(r.Context(), doc.From(r), doc.From(r).Caller().Name(), false)
```

Last, the API route the operation names. In `routes.go`, after the `POST /ui/cheers` line:

```go
	mux.HandleFunc("GET /api/cheers", listCheers)
	mux.HandleFunc("POST /api/cheers", cheerFromAPI)
```

and at the end:

```go
func listCheers(w http.ResponseWriter, r *http.Request) {
	cheers, next, err := doc.From(r).Find(r.Context(), doc.Query{Collection: "cheers", Order: doc.Newest, Limit: 50, After: r.URL.Query().Get("after")})
	if err != nil {
		doc.Problem(w, http.StatusServiceUnavailable, "unavailable", err.Error())
		return
	}
	doc.JSON(w, http.StatusOK, map[string]any{"cheers": cheers, "next": next})
}

// cheerFromAPI is the operation automations call by name.
func cheerFromAPI(w http.ResponseWriter, r *http.Request) {
	outcome, err := cheer(r.Context(), doc.From(r), doc.From(r).Caller().Name(), false)
	if err != nil {
		doc.Problem(w, http.StatusUnprocessableEntity, "not-cheered", err.Error())
		return
	}
	doc.JSON(w, http.StatusOK, outcome)
}
```

Everything under `api/` is also DOC's API, at `/api/v1/plugins/{{ values.name }}/api/`, for
anyone with a DOC access token who may use the plugin.

**Check:** run the plugin as the schedule would, from DOC's API:

```sh
curl -X POST -H "Authorization: Bearer $DOC_TOKEN" -H 'content-type: application/json' \
    -d '{"payload":{"schedule":"morning"}}' {{ platform.url }}/api/v1/plugins/{{ values.name }}/run
```

It answers `Already cheered up today.` if the button already did, and `curl -H "Authorization:
Bearer $DOC_TOKEN" {{ platform.url }}/api/v1/plugins/{{ values.name }}/api/cheers` lists the
cheers as JSON.

## 9. Refuse settings that would not work

DOC checks a setting against what its declaration allows — a number, a range, a cron expression —
before the plugin sees it. What only the plugin knows, it checks itself: DOC asks it before
anything is saved. A login nobody signs in with, or a team written without `team:`, would fail
every morning in silence; refuse them where they are typed instead. In `main.go`, above
`defaultMessages`:

```go
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
```

hand it to DOC in `main`, after `Run`:

```go
		// Asked before anybody's settings are saved.
		CheckSettings: checkSettings,
```

and add `"context"` and `"strings"` to `main.go`'s imports.

**Check:** on the Settings page, put `nobody` in **Who to cheer up**: it is refused, with
**nobody in DOC signs in as nobody** under the field, and nothing is saved.

## 10. Check your work

- `go vet ./...` finds nothing, and `gofmt -l .` lists nothing.
- Your files match `finished/`, apart from the comment at the top of `main.go` and perhaps a blank
  line or two: `diff main.go finished/main.go`, and the same for `routes.go`, `weather.go`,
  `person.go`, `calendar.go`, `cheer.go` and `pages/home.html`.
- The page shows the sky, and pressing the button cheers somebody up once a press; the scheduled run
  once a day.
- The Settings page refuses a login that is nobody's.
- **Admin → Plugins** shows {{ values.name }} as running. Stop it with Ctrl-C: within half a
  minute it is in error, and it is running again soon after `make run`.

## 11. Where next

- **Deploy a new version beside the old one.** Change something, then
  `make run VERSION=0.2.0 PORT=4491` while 0.1.0 is still running. DOC loads 0.2.0, moves every
  request over to it, and tells 0.1.0 to exit — the page never goes away.
- **Listen for events.** `Subscriptions: []string{"plugin.github.pull-request.merged"}` in the
  manifest, and an `OnEvent` in `main`, and it can cheer somebody up after every merge instead.
- **Run it in a container.** `make image` builds one. Mount DOC's secrets read-only at `/secrets`,
  join the network DOC's backend is on, and set `DOC_BACKEND_QUIC` to where it listens.
- **Read the reference.** DOC's `DOC-SPEC.md` is everything the protocol does, and `docs/plugin-authoring.md`
  is the guide to writing plugins; `internal/doc` follows both, section by section.
