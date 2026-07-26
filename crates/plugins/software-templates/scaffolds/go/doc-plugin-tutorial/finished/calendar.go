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
// are, with the person invited. It asks the calendar-events plugin through its peer/ routes, as
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
