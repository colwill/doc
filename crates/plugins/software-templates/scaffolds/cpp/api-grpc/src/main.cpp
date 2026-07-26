// {{ values.name }} — {{ values.description }}
#include <csignal>
#include <iostream>
#include <memory>
#include <string>

#include <grpcpp/ext/proto_server_reflection_plugin.h>
#include <grpcpp/grpcpp.h>
#include <grpcpp/health_check_service_interface.h>

#include "{{ scaffold.package }}/config.hpp"
#include "{{ scaffold.package }}/flags.hpp"
#include "{{ scaffold.package }}/service.hpp"
#include "{{ scaffold.package }}/telemetry.hpp"

namespace {
grpc::Server* serving = nullptr;

void stop(int) {
  if (serving != nullptr) serving->Shutdown();
}
}  // namespace

int main() {
  const auto config = {{ scaffold.package }}::Config::load();
  const {{ scaffold.package }}::Telemetry telemetry(config);
  {{ scaffold.package }}::Flags flags(config);
  flags.start();

  // Health and reflection are what everything else expects a gRPC service to answer.
  grpc::EnableDefaultHealthCheckService(true);
  grpc::reflection::InitProtoReflectionServerBuilderPlugin();

  {{ scaffold.package }}::Service service(config, flags, telemetry);
  const std::string address = config.address + ":9090";

  grpc::ServerBuilder builder;
  builder.AddListeningPort(address, grpc::InsecureServerCredentials());
  builder.RegisterService(&service);
  std::unique_ptr<grpc::Server> server(builder.BuildAndStart());
  if (!server) {
    std::cerr << "the port could not be taken: " << address << '\n';
    return 1;
  }

  serving = server.get();
  std::signal(SIGINT, stop);
  std::signal(SIGTERM, stop);

  std::cout << "listening on " << address << " as " << config.service << '\n';
  server->Wait();
  std::cout << "stopping\n";
  return 0;
}
