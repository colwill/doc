// Reconciling {{ scaffold.kind }}s: look at what was asked for, look at what the cluster has, and
// make the second match the first.
#pragma once

#include <nlohmann/json.hpp>

#include "{{ scaffold.package }}/cluster.hpp"
#include "{{ scaffold.package }}/config.hpp"
#include "{{ scaffold.package }}/flags.hpp"
#include "{{ scaffold.package }}/telemetry.hpp"

namespace {{ scaffold.package }} {

inline constexpr const char* kGroup = "{{ scaffold.group }}";
inline constexpr const char* kVersion = "v1alpha1";
inline constexpr const char* kPlural = "{{ scaffold.plural }}";

class Reconciler {
 public:
  Reconciler(const Cluster& cluster, const Config& config, const Flags& flags,
             const Telemetry& telemetry);

  // Brings one {{ scaffold.kind }} to what it asks for.
  void reconcile(const nlohmann::json& wanted) const;

  // Lists what there is, reconciles all of it, then watches for changes. Returns when the watch
  // ends, which is normal; the caller starts it again.
  void run_once() const;

 private:
  const Cluster& cluster_;
  const Config& config_;
  const Flags& flags_;
  const Telemetry& telemetry_;
};

}  // namespace {{ scaffold.package }}
