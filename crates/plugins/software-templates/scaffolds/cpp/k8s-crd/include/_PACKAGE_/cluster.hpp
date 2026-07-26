// A small Kubernetes client: enough to list, watch and patch one kind of resource. It reads the
// service account the pod is given, so nothing has to be configured where it runs.
#pragma once

#include <functional>
#include <optional>
#include <string>

#include <nlohmann/json.hpp>

namespace {{ scaffold.package }} {

class Cluster {
 public:
  // The API server as a pod sees it, with the token and the CA the service account mounts.
  static std::optional<Cluster> in_cluster();

  // Every {{ scaffold.kind }} there is now, and the version to watch on from.
  nlohmann::json list(const std::string& group, const std::string& version,
                      const std::string& plural) const;

  // Calls `on_event` for every change until the connection ends, which is normal: the caller
  // lists again and watches on from the version it was given.
  void watch(const std::string& group, const std::string& version, const std::string& plural,
             const std::string& from,
             const std::function<void(const std::string&, const nlohmann::json&)>& on_event) const;

  // A merge patch, which is how a controller writes what it has decided.
  bool patch(const std::string& path, const nlohmann::json& body,
             const std::string& content_type = "application/merge-patch+json") const;

  bool put(const std::string& path, const nlohmann::json& body) const;
  bool post(const std::string& path, const nlohmann::json& body) const;
  nlohmann::json get(const std::string& path) const;

 private:
  std::string host_;
  std::string token_;
  std::string authority_;
};

}  // namespace {{ scaffold.package }}
