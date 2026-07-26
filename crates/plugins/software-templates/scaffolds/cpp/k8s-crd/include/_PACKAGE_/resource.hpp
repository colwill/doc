// The custom resource, as C++ sees it. The CRD in config/crd/bases is the schema the cluster
// enforces; this is the same thing for the code that reads and writes them, and `validate` is the
// same rules again so a wrong one is refused before the API server has to refuse it.
#pragma once

#include <map>
#include <optional>
#include <string>
#include <vector>

#include <nlohmann/json.hpp>

namespace {{ scaffold.package }} {

inline constexpr const char* kGroup = "{{ scaffold.group }}";
inline constexpr const char* kVersion = "v1alpha1";
inline constexpr const char* kKind = "{{ scaffold.kind }}";
inline constexpr const char* kPlural = "{{ scaffold.plural }}";

enum class Phase { Pending, Ready, Failed };

std::string as_text(Phase phase);
Phase phase_of(const std::string& text);

// What somebody asks for.
struct Spec {
  std::string owner;                       // the team answering for it
  int size = 1;                            // how many of them to run
  std::string retention = "168h";          // how long what it makes is kept
  std::map<std::string, std::string> settings;

  // Everything wrong with it, in the words the CRD's schema would use.
  std::vector<std::string> problems() const;
};

// What is actually true of it.
struct Status {
  Phase phase = Phase::Pending;
  std::optional<std::string> reason;
  long observed_generation = 0;
};

// A whole resource, as the API server returns it.
struct Resource {
  std::string name;
  std::string space = "default";
  Spec spec;
  Status status;

  static Resource from_json(const nlohmann::json& body);
  nlohmann::json to_json() const;
};

}  // namespace {{ scaffold.package }}
