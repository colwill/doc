// {{ values.name }} — {{ values.description }}
#include <csignal>
#include <iostream>

#include <httplib.h>

#include "{{ scaffold.package }}/api.hpp"
#include "{{ scaffold.package }}/config.hpp"
#include "{{ scaffold.package }}/flags.hpp"
#include "{{ scaffold.package }}/telemetry.hpp"

namespace {
httplib::Server* serving = nullptr;

void stop(int) {
  if (serving != nullptr) serving->stop();
}
}  // namespace

int main() {
  const auto config = {{ scaffold.package }}::Config::load();
  const {{ scaffold.package }}::Telemetry telemetry(config);
  {{ scaffold.package }}::Flags flags(config);
  flags.start();

  httplib::Server server;
  serving = &server;
  std::signal(SIGINT, stop);
  std::signal(SIGTERM, stop);

  {{ scaffold.package }}::routes(server, config, flags, telemetry);

  std::cout << "listening on " << config.address << ':' << config.port << " as " << config.service
            << '\n';
  if (!server.listen(config.address, config.port)) {
    std::cerr << "the port could not be taken\n";
    return 1;
  }
  std::cout << "stopping\n";
  return 0;
}
