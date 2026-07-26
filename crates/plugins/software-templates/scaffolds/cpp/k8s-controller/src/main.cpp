// {{ values.name }} — {{ values.description }}
//
// It lists every {{ scaffold.kind }}, reconciles them, then watches for changes. A watch that ends
// is not a failure: it lists again and carries on, which is how every Kubernetes controller works.
#include <atomic>
#include <chrono>
#include <csignal>
#include <iostream>
#include <thread>

#include "{{ scaffold.package }}/cluster.hpp"
#include "{{ scaffold.package }}/config.hpp"
#include "{{ scaffold.package }}/flags.hpp"
#include "{{ scaffold.package }}/reconciler.hpp"
#include "{{ scaffold.package }}/telemetry.hpp"

namespace {
std::atomic<bool> stopping{false};

void stop(int) { stopping = true; }
}  // namespace

int main() {
  std::signal(SIGINT, stop);
  std::signal(SIGTERM, stop);

  const auto config = {{ scaffold.package }}::Config::load();
  const {{ scaffold.package }}::Telemetry telemetry(config);
  {{ scaffold.package }}::Flags flags(config);
  flags.start();

  const auto cluster = {{ scaffold.package }}::Cluster::in_cluster();
  if (!cluster.has_value()) return 1;

  const {{ scaffold.package }}::Reconciler reconciler(cluster.value(), config, flags, telemetry);
  std::cout << "watching {{ scaffold.kind }}s in {{ scaffold.group }} as " << config.service << '\n';

  while (!stopping) {
    reconciler.run_once();
    if (!stopping) std::this_thread::sleep_for(std::chrono::seconds(2));
  }
  std::cout << "stopping\n";
  return 0;
}
