// Command {{ values.name }} — {{ values.description }}
package main

import (
	"context"
	"log/slog"
	"net"
	"os"
	"os/signal"
	"syscall"

	"google.golang.org/grpc"
	"google.golang.org/grpc/health"
	"google.golang.org/grpc/health/grpc_health_v1"
	"google.golang.org/grpc/reflection"
	"google.golang.org/grpc/stats/opentelemetry"

	{{ scaffold.package }}v1 "{{ scaffold.module }}/gen/{{ scaffold.package }}/v1"
	"{{ scaffold.module }}/internal/platform"
	"{{ scaffold.module }}/internal/rpc"
)

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	slog.SetDefault(slog.New(slog.NewJSONHandler(os.Stdout, nil)))
	config := platform.Load()
	if config.Address == ":8080" {
		config.Address = ":9090"
	}

	telemetry, err := platform.StartTelemetry(ctx, config)
	if err != nil {
		slog.Error("telemetry could not be set up", "error", err)
		os.Exit(1)
	}
	defer func() { _ = telemetry.Shutdown(context.WithoutCancel(ctx)) }()
	flags := platform.StartFlags(ctx, config)

	listener, err := net.Listen("tcp", config.Address)
	if err != nil {
		slog.Error("the port could not be taken", "address", config.Address, "error", err)
		os.Exit(1)
	}

	// Every call is traced and counted through the same stack the rest of the platform exports to.
	server := grpc.NewServer(opentelemetry.ServerOption(opentelemetry.Options{}))
	{{ scaffold.package }}v1.Register{{ scaffold.pascal }}ServiceServer(server, rpc.New(config, flags))

	checks := health.NewServer()
	grpc_health_v1.RegisterHealthServer(server, checks)
	checks.SetServingStatus("", grpc_health_v1.HealthCheckResponse_SERVING)
	reflection.Register(server)

	go func() {
		slog.Info("listening", "address", config.Address, "service", config.Service)
		if err := server.Serve(listener); err != nil {
			slog.Error("the server stopped", "error", err)
			stop()
		}
	}()

	<-ctx.Done()
	slog.Info("stopping")
	checks.SetServingStatus("", grpc_health_v1.HealthCheckResponse_NOT_SERVING)
	server.GracefulStop()
}
