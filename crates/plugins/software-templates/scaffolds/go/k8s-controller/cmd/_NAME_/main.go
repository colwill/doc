// Command {{ values.name }} — {{ values.description }}
package main

import (
	"context"
	"log/slog"
	"os"

	"k8s.io/apimachinery/pkg/runtime"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/healthz"
	"sigs.k8s.io/controller-runtime/pkg/log/zap"
	metricsserver "sigs.k8s.io/controller-runtime/pkg/metrics/server"

	"{{ scaffold.module }}/api/v1alpha1"
	"{{ scaffold.module }}/internal/controller"
	"{{ scaffold.module }}/internal/platform"
)

func main() {
	ctx := ctrl.SetupSignalHandler()
	ctrl.SetLogger(zap.New(zap.UseDevMode(false)))

	config := platform.Load()
	telemetry, err := platform.StartTelemetry(ctx, config)
	if err != nil {
		slog.Error("telemetry could not be set up", "error", err)
		os.Exit(1)
	}
	defer func() { _ = telemetry.Shutdown(context.WithoutCancel(ctx)) }()
	flags := platform.StartFlags(ctx, config)

	scheme := runtime.NewScheme()
	if err := clientgoscheme.AddToScheme(scheme); err != nil {
		slog.Error("the scheme could not be built", "error", err)
		os.Exit(1)
	}
	if err := v1alpha1.AddToScheme(scheme); err != nil {
		slog.Error("the scheme could not be built", "error", err)
		os.Exit(1)
	}

	manager, err := ctrl.NewManager(ctrl.GetConfigOrDie(), ctrl.Options{
		Scheme:                 scheme,
		Metrics:                metricsserver.Options{BindAddress: ":8081"},
		HealthProbeBindAddress: ":8080",
		LeaderElection:         true,
		LeaderElectionID:       "{{ values.name }}.{{ scaffold.group }}",
	})
	if err != nil {
		slog.Error("the manager could not be started", "error", err)
		os.Exit(1)
	}

	reconciler := &controller.Reconciler{
		Client: manager.GetClient(),
		Scheme: manager.GetScheme(),
		Config: config,
		Flags:  flags,
	}
	if err := reconciler.SetupWithManager(manager); err != nil {
		slog.Error("the controller could not be set up", "error", err)
		os.Exit(1)
	}
	if err := manager.AddHealthzCheck("healthz", healthz.Ping); err != nil {
		slog.Error("the health check could not be added", "error", err)
		os.Exit(1)
	}
	if err := manager.AddReadyzCheck("readyz", healthz.Ping); err != nil {
		slog.Error("the readiness check could not be added", "error", err)
		os.Exit(1)
	}

	slog.Info("watching", "kind", "{{ scaffold.kind }}", "group", "{{ scaffold.group }}")
	if err := manager.Start(ctx); err != nil {
		slog.Error("the manager stopped", "error", err)
		os.Exit(1)
	}
}
