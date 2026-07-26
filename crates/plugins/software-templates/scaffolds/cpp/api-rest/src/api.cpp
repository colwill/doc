#include "{{ scaffold.package }}/api.hpp"

#include <chrono>
#include <iostream>

#include <nlohmann/json.hpp>

namespace {{ scaffold.package }} {

void routes(httplib::Server& server, const Config& config, const Flags& flags,
            const Telemetry& telemetry) {
  auto requests = telemetry.meter()->CreateUInt64Counter("http.server.requests",
                                                         "Requests this service answered");

  // Liveness: the process is up. Kubernetes restarts it when this stops answering.
  server.Get("/healthz", [](const httplib::Request&, httplib::Response& response) {
    nlohmann::json body;
    body["status"] = "ok";
    response.set_content(body.dump(), "application/json");
  });

  // Readiness: it is up and willing to take traffic, which a flag can withdraw.
  server.Get("/readyz", [&flags](const httplib::Request&, httplib::Response& response) {
    nlohmann::json body;
    const bool taking = flags.boolean("accepting-traffic", true);
    body["status"] = taking ? "ready" : "draining";
    response.status = taking ? 200 : 503;
    response.set_content(body.dump(), "application/json");
  });

  server.Get("/api/v1/hello", [&config, &flags, &telemetry, requests](const httplib::Request&,
                                                                     httplib::Response& response) {
    auto span = telemetry.tracer()->StartSpan("hello");
    requests->Add(1);

    // What the service says is decided in DOC, with a fallback it keeps working on.
    nlohmann::json body;
    body["message"] = flags.string("greeting", "Hello");
    body["service"] = config.service;
    body["environment"] = config.environment;
    response.set_content(body.dump(), "application/json");
    span->End();
  });

  server.set_logger([](const httplib::Request& request, const httplib::Response& response) {
    std::cout << request.method << ' ' << request.path << ' ' << response.status << '\n';
  });
}

}  // namespace {{ scaffold.package }}
