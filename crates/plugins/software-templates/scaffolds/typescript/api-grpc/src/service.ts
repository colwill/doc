// {{ values.name }}'s gRPC service: what answers the contract in proto/service.proto.

import type * as grpc from "@grpc/grpc-js";
import { trace } from "@opentelemetry/api";

import type { Config, Flags } from "./platform/index.js";

const tracer = trace.getTracer("{{ values.name }}");

interface GreetRequest {
  who?: string;
}

interface GreetResponse {
  message: string;
  service: string;
  environment: string;
}

type Handler = grpc.handleUnaryCall<GreetRequest, GreetResponse>;

/** Greet answers with what this service is told to say, which DOC decides at runtime. */
export function greet(config: Config, flags: Flags): Handler {
  return (call, callback) => {
    tracer.startActiveSpan("greet", (span) => {
      const who = call.request.who === undefined || call.request.who === "" ? "world" : call.request.who;

      // What it says is a flag in DOC, with the value this service falls back to beside it.
      let greeting = flags.string("greeting", "Hello");
      if (flags.boolean("shout", false)) greeting = greeting.toUpperCase();

      span.end();
      callback(null, {
        message: `${greeting}, ${who}`,
        service: config.service,
        environment: config.environment,
      });
    });
  };
}
