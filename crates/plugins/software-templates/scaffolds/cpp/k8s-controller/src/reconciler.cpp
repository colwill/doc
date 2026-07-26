#include "{{ scaffold.package }}/reconciler.hpp"

#include <iostream>
#include <string>

namespace {{ scaffold.package }} {

Reconciler::Reconciler(const Cluster& cluster, const Config& config, const Flags& flags,
                       const Telemetry& telemetry)
    : cluster_(cluster), config_(config), flags_(flags), telemetry_(telemetry) {}

void Reconciler::reconcile(const nlohmann::json& wanted) const {
  auto span = telemetry_.tracer()->StartSpan("reconcile");

  const auto metadata = wanted.value("metadata", nlohmann::json::object());
  const std::string name = metadata.value("name", "");
  const std::string space = metadata.value("namespace", "default");
  const std::string uid = metadata.value("uid", "");
  if (name.empty()) return;

  // Turned off in DOC, this controller watches without changing anything: a way to stop it acting
  // without stopping it running.
  if (!flags_.boolean("reconcile", true)) {
    std::cout << "reconciling is turned off in DOC; nothing was changed\n";
    return;
  }

  const auto spec = wanted.value("spec", nlohmann::json::object());
  std::string message = spec.value("message", "");
  if (message.empty()) {
    message = flags_.string("default-message", "Made by {{ values.name }}");
  }

  // The owner reference is what makes the config map go when the resource does.
  nlohmann::json owner;
  owner["apiVersion"] = std::string(kGroup) + "/" + kVersion;
  owner["kind"] = "{{ scaffold.kind }}";
  owner["name"] = name;
  owner["uid"] = uid;
  owner["controller"] = true;

  nlohmann::json map;
  map["apiVersion"] = "v1";
  map["kind"] = "ConfigMap";
  map["metadata"]["name"] = name;
  map["metadata"]["namespace"] = space;
  map["metadata"]["ownerReferences"] = nlohmann::json::array({owner});
  map["data"]["message"] = message;
  map["data"]["size"] = std::to_string(spec.value("size", 1));

  const std::string maps = "/api/v1/namespaces/" + space + "/configmaps";
  if (!cluster_.put(maps + "/" + name, map) && !cluster_.post(maps, map)) {
    std::cerr << "the config map for " << space << '/' << name << " could not be written\n";
    return;
  }

  nlohmann::json status;
  status["status"]["ready"] = true;
  status["status"]["reason"] = "Everything asked for is in place";
  status["status"]["observedGeneration"] = metadata.value("generation", 0);
  const std::string path = "/apis/" + std::string(kGroup) + "/" + kVersion + "/namespaces/" + space +
                           "/" + kPlural + "/" + name + "/status";
  cluster_.patch(path, status);
  std::cout << space << '/' << name << " is in place\n";
}

void Reconciler::run_once() const {
  const auto held = cluster_.list(kGroup, kVersion, kPlural);
  if (held.is_null()) return;
  for (const auto& one : held.value("items", nlohmann::json::array())) {
    reconcile(one);
  }
  const std::string from = held.value("metadata", nlohmann::json::object()).value("resourceVersion", "0");
  std::cout << "watching from " << from << '\n';
  cluster_.watch(kGroup, kVersion, kPlural, from,
                 [this](const std::string& kind, const nlohmann::json& object) {
                   if (kind == "ADDED" || kind == "MODIFIED") reconcile(object);
                 });
}

}  // namespace {{ scaffold.package }}
