#include "{{ scaffold.package }}/resource.hpp"

#include <regex>

namespace {{ scaffold.package }} {

std::string as_text(Phase phase) {
  switch (phase) {
    case Phase::Ready:
      return "Ready";
    case Phase::Failed:
      return "Failed";
    case Phase::Pending:
    default:
      return "Pending";
  }
}

Phase phase_of(const std::string& text) {
  if (text == "Ready") return Phase::Ready;
  if (text == "Failed") return Phase::Failed;
  return Phase::Pending;
}

std::vector<std::string> Spec::problems() const {
  std::vector<std::string> found;
  static const std::regex owner_shape("^[a-z][a-z0-9-]{1,62}$");
  static const std::regex retention_shape("^[0-9]+(h|m|s)$");
  if (!std::regex_match(owner, owner_shape)) {
    found.push_back("owner is a team name: lowercase letters, digits and dashes");
  }
  if (size < 0 || size > 100) {
    found.push_back("size is between 0 and 100");
  }
  if (!retention.empty() && !std::regex_match(retention, retention_shape)) {
    found.push_back("retention is a span such as 168h, 30m or 45s");
  }
  return found;
}

Resource Resource::from_json(const nlohmann::json& body) {
  Resource resource;
  const auto metadata = body.value("metadata", nlohmann::json::object());
  resource.name = metadata.value("name", "");
  resource.space = metadata.value("namespace", "default");

  const auto spec = body.value("spec", nlohmann::json::object());
  resource.spec.owner = spec.value("owner", "");
  resource.spec.size = spec.value("size", 1);
  resource.spec.retention = spec.value("retention", "168h");
  if (spec.contains("settings") && spec.at("settings").is_object()) {
    for (const auto& [key, value] : spec.at("settings").items()) {
      resource.spec.settings.emplace(key, value.get<std::string>());
    }
  }

  const auto status = body.value("status", nlohmann::json::object());
  resource.status.phase = phase_of(status.value("phase", "Pending"));
  if (status.contains("reason")) resource.status.reason = status.value("reason", "");
  resource.status.observed_generation = status.value("observedGeneration", 0);
  return resource;
}

nlohmann::json Resource::to_json() const {
  nlohmann::json body;
  body["apiVersion"] = std::string(kGroup) + "/" + kVersion;
  body["kind"] = kKind;
  body["metadata"]["name"] = name;
  body["metadata"]["namespace"] = space;
  body["spec"]["owner"] = spec.owner;
  body["spec"]["size"] = spec.size;
  body["spec"]["retention"] = spec.retention;
  if (!spec.settings.empty()) {
    for (const auto& [key, value] : spec.settings) body["spec"]["settings"][key] = value;
  }
  return body;
}

}  // namespace {{ scaffold.package }}
