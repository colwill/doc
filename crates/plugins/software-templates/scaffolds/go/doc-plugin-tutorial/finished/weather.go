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
