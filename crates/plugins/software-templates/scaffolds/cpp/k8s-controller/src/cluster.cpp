#include "{{ scaffold.package }}/cluster.hpp"

#include <cstdlib>
#include <fstream>
#include <iostream>
#include <sstream>

#include <cpr/cpr.h>

namespace {{ scaffold.package }} {
namespace {

constexpr const char* kTokenPath = "/var/run/secrets/kubernetes.io/serviceaccount/token";
constexpr const char* kAuthorityPath = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";

std::string read_file(const std::string& path) {
  std::ifstream file(path);
  if (!file) return {};
  std::ostringstream held;
  held << file.rdbuf();
  return held.str();
}

std::string environment(const char* name, const std::string& fallback) {
  const char* value = std::getenv(name);
  return (value != nullptr && *value != '\0') ? std::string(value) : fallback;
}

}  // namespace

std::optional<Cluster> Cluster::in_cluster() {
  Cluster cluster;
  const std::string host = environment("KUBERNETES_SERVICE_HOST", "kubernetes.default.svc");
  const std::string port = environment("KUBERNETES_SERVICE_PORT", "443");
  cluster.host_ = "https://" + host + ":" + port;
  cluster.token_ = read_file(kTokenPath);
  cluster.authority_ = kAuthorityPath;
  if (cluster.token_.empty()) {
    std::cerr << "there is no service account token at " << kTokenPath
              << ": this is meant to run in a cluster\n";
    return std::nullopt;
  }
  return cluster;
}

nlohmann::json Cluster::get(const std::string& path) const {
  const cpr::Response answer =
      cpr::Get(cpr::Url(host_ + path), cpr::Bearer(token_), cpr::SslOptions(cpr::ssl::CaInfo(authority_)),
               cpr::Timeout(15000));
  if (answer.status_code != 200) {
    std::cerr << "the API server answered " << answer.status_code << " to " << path << '\n';
    return nlohmann::json();
  }
  return nlohmann::json::parse(answer.text, nullptr, false);
}

nlohmann::json Cluster::list(const std::string& group, const std::string& version,
                             const std::string& plural) const {
  return get("/apis/" + group + "/" + version + "/" + plural);
}

void Cluster::watch(
    const std::string& group, const std::string& version, const std::string& plural,
    const std::string& from,
    const std::function<void(const std::string&, const nlohmann::json&)>& on_event) const {
  const std::string path = "/apis/" + group + "/" + version + "/" + plural +
                           "?watch=true&allowWatchBookmarks=true&resourceVersion=" + from;
  std::string partial;
  // The API server streams one JSON object a line for as long as the connection holds.
  cpr::Get(
      cpr::Url(host_ + path), cpr::Bearer(token_),
      cpr::SslOptions(cpr::ssl::CaInfo(authority_)),
      cpr::WriteCallback([&](const std::string_view& piece, intptr_t) -> bool {
        partial.append(piece);
        std::size_t line = partial.find('\n');
        while (line != std::string::npos) {
          const std::string one = partial.substr(0, line);
          partial.erase(0, line + 1);
          const auto event = nlohmann::json::parse(one, nullptr, false);
          if (!event.is_discarded() && event.contains("type") && event.contains("object")) {
            on_event(event.at("type").get<std::string>(), event.at("object"));
          }
          line = partial.find('\n');
        }
        return true;
      }));
}

bool Cluster::patch(const std::string& path, const nlohmann::json& body,
                    const std::string& content_type) const {
  cpr::Header headers;
  headers["content-type"] = content_type;
  const cpr::Response answer =
      cpr::Patch(cpr::Url(host_ + path), cpr::Bearer(token_), headers, cpr::Body(body.dump()),
                 cpr::SslOptions(cpr::ssl::CaInfo(authority_)), cpr::Timeout(15000));
  if (answer.status_code / 100 != 2) {
    std::cerr << "the API server refused a patch to " << path << ": " << answer.status_code << ' '
              << answer.text << '\n';
    return false;
  }
  return true;
}

bool Cluster::put(const std::string& path, const nlohmann::json& body) const {
  cpr::Header headers;
  headers["content-type"] = "application/json";
  const cpr::Response answer =
      cpr::Put(cpr::Url(host_ + path), cpr::Bearer(token_), headers, cpr::Body(body.dump()),
               cpr::SslOptions(cpr::ssl::CaInfo(authority_)), cpr::Timeout(15000));
  return answer.status_code / 100 == 2;
}

bool Cluster::post(const std::string& path, const nlohmann::json& body) const {
  cpr::Header headers;
  headers["content-type"] = "application/json";
  const cpr::Response answer =
      cpr::Post(cpr::Url(host_ + path), cpr::Bearer(token_), headers, cpr::Body(body.dump()),
                cpr::SslOptions(cpr::ssl::CaInfo(authority_)), cpr::Timeout(15000));
  if (answer.status_code / 100 != 2) {
    std::cerr << "the API server refused " << path << ": " << answer.status_code << ' '
              << answer.text << '\n';
    return false;
  }
  return true;
}

}  // namespace {{ scaffold.package }}
