// Command {{ values.name }} — {{ values.description }}
package main

import (
	"context"
	"log/slog"
	"os"
	"os/signal"
	"syscall"

	"{{ scaffold.module }}/internal/command"
	"{{ scaffold.module }}/internal/platform"
)

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	slog.SetDefault(slog.New(slog.NewJSONHandler(os.Stderr, nil)))
	config := platform.Load()

	telemetry, err := platform.StartTelemetry(ctx, config)
	if err != nil {
		// A command still runs when the collector is unreachable; it is only not measured.
		slog.Warn("telemetry is not being exported", "error", err)
	} else {
		defer func() { _ = telemetry.Shutdown(context.WithoutCancel(ctx)) }()
	}
	flags := platform.StartFlags(ctx, config)

	if err := command.Run(ctx, config, flags, os.Args[1:], os.Stdout); err != nil {
		slog.Error("it did not finish", "error", err)
		os.Exit(1)
	}
}
