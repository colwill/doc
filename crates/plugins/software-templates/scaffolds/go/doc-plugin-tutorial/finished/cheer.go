package main

import (
	"context"
	"fmt"
	"log/slog"
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
func cheer(ctx context.Context, b *doc.Backend, by string, oncePerDay bool) (Outcome, error) {
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

	person, err := findPerson(ctx, b, login)
	if err != nil {
		return Outcome{}, err
	}
	message := todays(settings.List(settingMessages), sky, place)

	// The break is a kindness, not the point: a calendar that is not there does not stop the message.
	if team := settings.Text(settingTeam); team != "" {
		outcome.Event, err = bookBreak(ctx, b, team, person, sky)
		if err != nil {
			slog.Warn("the sunshine break could not be booked", "error", err)
		}
	}
	link := outcome.Event
	if link == "" {
		link = "/p/" + b.ID() + "/"
	}
	if err := tell(ctx, b, person, "A little sunshine for a cloudy day", message, link); err != nil {
		return Outcome{}, err
	}

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
