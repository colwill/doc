// {{ values.name }}'s gRPC service: what answers the contract in proto/service.proto.
#pragma once

#include <grpcpp/grpcpp.h>

#include "service.grpc.pb.h"

#include "{{ scaffold.package }}/config.hpp"
#include "{{ scaffold.package }}/flags.hpp"
#include "{{ scaffold.package }}/telemetry.hpp"

namespace {{ scaffold.package }} {

class Service final : public {{ scaffold.package }}::v1::{{ scaffold.pascal }}Service::Service {
 public:
  Service(const Config& config, const Flags& flags, const Telemetry& telemetry);

  grpc::Status Greet(grpc::ServerContext* context,
                     const {{ scaffold.package }}::v1::GreetRequest* request,
                     {{ scaffold.package }}::v1::GreetResponse* response) override;

 private:
  const Config& config_;
  const Flags& flags_;
  const Telemetry& telemetry_;
};

}  // namespace {{ scaffold.package }}
