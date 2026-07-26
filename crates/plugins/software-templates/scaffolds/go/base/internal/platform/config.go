// Package platform holds what every service on this platform has: its settings, its telemetry and
// its feature flags. It was written by DOC when this service was created and is yours to change.
package platform

import (
	"os"
	"strconv"
	"time"
)

// Config is what the service reads from its environment at start-up. Anything that should change
// without a deployment belongs in Flags instead, which is read while the service runs.
type Config struct {
	Service     string
	Environment string
	Address     string
	OTLPEndpoint string
	OTLPProtocol string
	FlagsURL    string
	FlagsToken  string
	FlagsPoll   time.Duration
}

// Load reads the environment, falling back to what DOC knew when this service was created.
func Load() Config {
	return Config{
		Service:      env("OTEL_SERVICE_NAME", "{{ values.name }}"),
		Environment:  env("DOC_ENVIRONMENT", "{{ telemetry.environment }}"),
		Address:      env("ADDRESS", ":8080"),
		OTLPEndpoint: env("OTEL_EXPORTER_OTLP_ENDPOINT", "{{ telemetry.endpoint }}"),
		OTLPProtocol: env("OTEL_EXPORTER_OTLP_PROTOCOL", "{{ telemetry.protocol }}"),
		FlagsURL:     env("DOC_FLAGS_URL", "{{ flags.url }}"),
		FlagsToken:   os.Getenv("DOC_FLAGS_TOKEN"),
		FlagsPoll:    seconds("DOC_FLAGS_POLL_SECONDS", 30*time.Second),
	}
}

func env(name, fallback string) string {
	if value := os.Getenv(name); value != "" {
		return value
	}
	return fallback
}

func seconds(name string, fallback time.Duration) time.Duration {
	if value := os.Getenv(name); value != "" {
		if number, err := strconv.Atoi(value); err == nil && number > 0 {
			return time.Duration(number) * time.Second
		}
	}
	return fallback
}
