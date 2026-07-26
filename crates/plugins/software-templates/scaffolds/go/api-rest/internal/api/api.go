// Package api serves {{ values.name }}'s HTTP API. Every request is traced, counted and given a
// request id; the routes themselves are in Routes.
package api

import (
	"encoding/json"
	"log/slog"
	"net/http"
	"time"

	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/metric"

	"{{ scaffold.module }}/internal/platform"
)

// Routes is everything {{ values.name }} answers.
func Routes(config platform.Config, flags *platform.Flags) http.Handler {
	meter := otel.Meter(config.Service)
	requests, _ := meter.Int64Counter("http.server.requests",
		metric.WithDescription("Requests this service answered"))
	latency, _ := meter.Float64Histogram("http.server.duration",
		metric.WithDescription("How long it took to answer"), metric.WithUnit("ms"))

	mux := http.NewServeMux()

	// Liveness: the process is up. Kubernetes restarts it when this stops answering.
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, r *http.Request) {
		write(w, http.StatusOK, map[string]any{"status": "ok"})
	})

	// Readiness: it is up and willing to take traffic.
	mux.HandleFunc("GET /readyz", func(w http.ResponseWriter, r *http.Request) {
		if !flags.Bool("accepting-traffic", true) {
			write(w, http.StatusServiceUnavailable, map[string]any{"status": "draining"})
			return
		}
		write(w, http.StatusOK, map[string]any{"status": "ready"})
	})

	mux.HandleFunc("GET /api/v1/hello", func(w http.ResponseWriter, r *http.Request) {
		_, span := otel.Tracer(config.Service).Start(r.Context(), "hello")
		defer span.End()

		// What the service does is decided by DOC, with a fallback it keeps working on.
		greeting := flags.String("greeting", "Hello")
		span.SetAttributes(attribute.String("greeting", greeting))
		write(w, http.StatusOK, map[string]any{
			"message":     greeting,
			"service":     config.Service,
			"environment": config.Environment,
		})
	})

	return measured(mux, config, requests, latency)
}

// measured wraps the routes so every answer is traced and counted, whatever it is.
func measured(next http.Handler, config platform.Config, requests metric.Int64Counter, latency metric.Float64Histogram) http.Handler {
	tracer := otel.Tracer(config.Service)
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		started := time.Now()
		ctx, span := tracer.Start(r.Context(), r.Method+" "+r.URL.Path)
		defer span.End()

		recorder := &recorder{ResponseWriter: w, status: http.StatusOK}
		next.ServeHTTP(recorder, r.WithContext(ctx))

		attributes := metric.WithAttributes(
			attribute.String("http.request.method", r.Method),
			attribute.String("http.route", r.URL.Path),
			attribute.Int("http.response.status_code", recorder.status),
		)
		requests.Add(ctx, 1, attributes)
		latency.Record(ctx, float64(time.Since(started).Microseconds())/1000, attributes)
		span.SetAttributes(attribute.Int("http.response.status_code", recorder.status))
		slog.InfoContext(ctx, "answered",
			"method", r.Method, "path", r.URL.Path, "status", recorder.status,
			"ms", time.Since(started).Milliseconds())
	})
}

type recorder struct {
	http.ResponseWriter
	status int
}

func (r *recorder) WriteHeader(status int) {
	r.status = status
	r.ResponseWriter.WriteHeader(status)
}

func write(w http.ResponseWriter, status int, body any) {
	w.Header().Set("content-type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(body)
}
