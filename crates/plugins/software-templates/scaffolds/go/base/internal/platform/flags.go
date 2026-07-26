package platform

import (
	"context"
	"encoding/json"
	"log/slog"
	"net/http"
	"sync"
	"time"
)

// Flags are this service's feature flags and runtime configuration, read from DOC. Everything that
// applies to the service is read in one call and kept here; the call carries an ETag, so a poll
// that finds nothing new costs a 304 and nothing else.
//
// Every read takes the value the service falls back to, so a service whose flags cannot be reached
// keeps running on its defaults rather than stopping.
type Flags struct {
	config  Config
	client  *http.Client
	mutex   sync.RWMutex
	values  answer
	version string
}

type answer struct {
	Flags          map[string]any `json:"flags"`
	Config         map[string]any `json:"config"`
	Version        string         `json:"version"`
	RefreshSeconds int            `json:"refresh_seconds"`
}

// StartFlags reads the flags once and then keeps them up to date until ctx is done.
func StartFlags(ctx context.Context, config Config) *Flags {
	flags := &Flags{
		config: config,
		client: &http.Client{Timeout: 5 * time.Second},
		values: answer{Flags: map[string]any{}, Config: map[string]any{}},
	}
	if err := flags.read(ctx); err != nil {
		slog.Warn("the flags could not be read; the service is running on its defaults", "error", err)
	}
	go flags.poll(ctx)
	return flags
}

func (f *Flags) poll(ctx context.Context) {
	wait := f.config.FlagsPoll
	for {
		select {
		case <-ctx.Done():
			return
		case <-time.After(wait):
			if err := f.read(ctx); err != nil {
				slog.Warn("the flags could not be read", "error", err)
			}
			f.mutex.RLock()
			if f.values.RefreshSeconds > 0 {
				wait = time.Duration(f.values.RefreshSeconds) * time.Second
			}
			f.mutex.RUnlock()
		}
	}
}

func (f *Flags) read(ctx context.Context) error {
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, f.config.FlagsURL, nil)
	if err != nil {
		return err
	}
	query := request.URL.Query()
	query.Set("service", f.config.Service)
	query.Set("environment", f.config.Environment)
	request.URL.RawQuery = query.Encode()
	if f.config.FlagsToken != "" {
		request.Header.Set("authorization", "Bearer "+f.config.FlagsToken)
	}
	f.mutex.RLock()
	version := f.version
	f.mutex.RUnlock()
	if version != "" {
		request.Header.Set("if-none-match", `"`+version+`"`)
	}

	response, err := f.client.Do(request)
	if err != nil {
		return err
	}
	defer response.Body.Close()
	if response.StatusCode == http.StatusNotModified {
		return nil
	}
	if response.StatusCode != http.StatusOK {
		return &httpError{status: response.StatusCode}
	}
	var read answer
	if err := json.NewDecoder(response.Body).Decode(&read); err != nil {
		return err
	}
	f.mutex.Lock()
	f.values, f.version = read, read.Version
	f.mutex.Unlock()
	return nil
}

type httpError struct{ status int }

func (e *httpError) Error() string { return http.StatusText(e.status) }

func (f *Flags) value(key string) (any, bool) {
	f.mutex.RLock()
	defer f.mutex.RUnlock()
	if value, held := f.values.Flags[key]; held {
		return value, true
	}
	value, held := f.values.Config[key]
	return value, held
}

// Bool is a switch: on, off, or the fallback when the platform holds nothing for this service.
func (f *Flags) Bool(key string, fallback bool) bool {
	if value, held := f.value(key); held {
		if answer, ok := value.(bool); ok {
			return answer
		}
	}
	return fallback
}

// String is a value read while the service runs, such as a message or a mode.
func (f *Flags) String(key string, fallback string) string {
	if value, held := f.value(key); held {
		if answer, ok := value.(string); ok {
			return answer
		}
	}
	return fallback
}

// Int is a number read while the service runs, such as a limit or a timeout in seconds.
func (f *Flags) Int(key string, fallback int) int {
	if value, held := f.value(key); held {
		if answer, ok := value.(float64); ok {
			return int(answer)
		}
	}
	return fallback
}

// JSON fills target with a structured value, and says whether it held one.
func (f *Flags) JSON(key string, target any) bool {
	value, held := f.value(key)
	if !held {
		return false
	}
	body, err := json.Marshal(value)
	if err != nil {
		return false
	}
	return json.Unmarshal(body, target) == nil
}
