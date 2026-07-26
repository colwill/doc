// {{ values.name }} — {{ values.description }}
//
// The custom resource lives in resource.hpp; this reads and writes them, so the type is used by
// something from the first commit.
//
//   {{ values.name }} list
//   {{ values.name }} create my-thing
#include <iostream>
#include <string>
#include <vector>

#include "{{ scaffold.package }}/cluster.hpp"
#include "{{ scaffold.package }}/config.hpp"
#include "{{ scaffold.package }}/flags.hpp"
#include "{{ scaffold.package }}/resource.hpp"
#include "{{ scaffold.package }}/telemetry.hpp"

int main(int argc, char** argv) {
  const std::vector<std::string> arguments(argv + 1, argv + argc);
  const std::string command = arguments.empty() ? "list" : arguments[0];

  const auto config = {{ scaffold.package }}::Config::load();
  const {{ scaffold.package }}::Telemetry telemetry(config);
  {{ scaffold.package }}::Flags flags(config);
  flags.start();

  const auto cluster = {{ scaffold.package }}::Cluster::in_cluster();
  if (!cluster.has_value()) return 1;
  const std::string space = "default";

  if (command == "create") {
    {{ scaffold.package }}::Resource wanted;
    wanted.name = arguments.size() > 1 ? arguments[1] : "{{ values.name }}-sample";
    wanted.space = space;
    wanted.spec.owner = flags.string("default-owner", "{{ values.team | name | default('platform') }}");
    wanted.spec.size = 1;

    const auto problems = wanted.spec.problems();
    if (!problems.empty()) {
      for (const auto& problem : problems) std::cerr << "that will be refused: " << problem << '\n';
      return 1;
    }

    const std::string path = "/apis/" + std::string({{ scaffold.package }}::kGroup) + "/" +
                             {{ scaffold.package }}::kVersion + "/namespaces/" + space + "/" +
                             {{ scaffold.package }}::kPlural;
    if (!cluster->post(path, wanted.to_json())) return 1;
    std::cout << "{{ scaffold.kind }}/" << wanted.name << " created in " << space << '\n';
    return 0;
  }

  const auto held = cluster->get("/apis/" + std::string({{ scaffold.package }}::kGroup) + "/" +
                                 {{ scaffold.package }}::kVersion + "/namespaces/" + space + "/" +
                                 {{ scaffold.package }}::kPlural);
  const auto items = held.value("items", nlohmann::json::array());
  if (items.empty()) {
    std::cout << "there are no {{ scaffold.plural }} in " << space << '\n';
    return 0;
  }
  for (const auto& one : items) {
    const auto resource = {{ scaffold.package }}::Resource::from_json(one);
    std::cout << resource.name << "\towner=" << resource.spec.owner
              << "\tsize=" << resource.spec.size
              << "\tphase=" << {{ scaffold.package }}::as_text(resource.status.phase) << '\n';
  }
  return 0;
}
