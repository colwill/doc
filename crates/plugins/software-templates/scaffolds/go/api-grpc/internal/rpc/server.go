// Package rpc implements {{ values.name }}'s gRPC service.
package rpc

import (
	"context"
	"strings"

	"go.opentelemetry.io/otel/trace"

	{{ scaffold.package }}v1 "{{ scaffold.module }}/gen/{{ scaffold.package }}/v1"
	"{{ scaffold.module }}/internal/platform"
)

// Server answers {{ scaffold.pascal }}Service.
type Server struct {
	{{ scaffold.package }}v1.Unimplemented{{ scaffold.pascal }}ServiceServer
	config platform.Config
	flags  *platform.Flags
}

func New(config platform.Config, flags *platform.Flags) *Server {
	return &Server{config: config, flags: flags}
}

func (s *Server) Greet(ctx context.Context, request *{{ scaffold.package }}v1.GreetRequest) (*{{ scaffold.package }}v1.GreetResponse, error) {
	span := trace.SpanFromContext(ctx)

	// What it says is a flag in DOC, with the value this service falls back to beside it.
	greeting := s.flags.String("greeting", "Hello")
	if s.flags.Bool("shout", false) {
		greeting = strings.ToUpper(greeting)
	}
	who := request.GetWho()
	if who == "" {
		who = "world"
	}
	span.AddEvent("greeted")

	return &{{ scaffold.package }}v1.GreetResponse{
		Message:     greeting + ", " + who,
		Service:     s.config.Service,
		Environment: s.config.Environment,
	}, nil
}
