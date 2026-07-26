#include "{{ scaffold.package }}/service.hpp"

#include <algorithm>
#include <cctype>
#include <string>

namespace {{ scaffold.package }} {

Service::Service(const Config& config, const Flags& flags, const Telemetry& telemetry)
    : config_(config), flags_(flags), telemetry_(telemetry) {}

grpc::Status Service::Greet(grpc::ServerContext*,
                            const {{ scaffold.package }}::v1::GreetRequest* request,
                            {{ scaffold.package }}::v1::GreetResponse* response) {
  auto span = telemetry_.tracer()->StartSpan("greet");

  std::string who = request->who();
  if (who.empty()) who = "world";

  // What it says is a flag in DOC, with the value this service falls back to beside it.
  std::string greeting = flags_.string("greeting", "Hello");
  if (flags_.boolean("shout", false)) {
    std::transform(greeting.begin(), greeting.end(), greeting.begin(),
                   [](unsigned char letter) { return std::toupper(letter); });
  }

  response->set_message(greeting + ", " + who);
  response->set_service(config_.service);
  response->set_environment(config_.environment);
  span->End();
  return grpc::Status::OK;
}

}  // namespace {{ scaffold.package }}
