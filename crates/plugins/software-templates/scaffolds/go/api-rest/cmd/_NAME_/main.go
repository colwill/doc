// Command {{ values.name }} — {{ values.description }}
package main

import (
	"context"
	"errors"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"{{ scaffold.module }}/internal/api"
	"{{ scaffold.module }}/internal/platform"
)

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	slog.SetDefault(slog.New(slog.NewJSONHandler(os.Stdout, nil)))
	config := platform.Load()

	telemetry, err := platform.StartTelemetry(ctx, config)
	if err != nil {
		slog.Error("telemetry could not be set up", "error", err)
		os.Exit(1)
	}
	flags := platform.StartFlags(ctx, config)

	server := &http.Server{
		Addr:              config.Address,
		Handler:           api.Routes(config, flags),
		ReadHeaderTimeout: 5 * time.Second,
	}

	go func() {
		slog.Info("listening", "address", config.Address, "service", config.Service)
		if err := server.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			slog.Error("the server stopped", "error", err)
			stop()
		}
	}()

	<-ctx.Done()
	slog.Info("stopping")
	closing, cancel := context.WithTimeout(context.WithoutCancel(ctx), 10*time.Second)
	defer cancel()
	_ = server.Shutdown(closing)
	_ = telemetry.Shutdown(closing)
}
