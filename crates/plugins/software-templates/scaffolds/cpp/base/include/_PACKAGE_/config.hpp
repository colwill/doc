// What the service reads from its environment at start-up. Anything that should change without a
// deployment belongs in flags.hpp instead, which is read while the service runs.
#pragma once

#include <chrono>
#include <optional>
#include <string>

namespace {{ scaffold.package }} {

struct Config {
  std::string service;
  std::string environment;
  std::string address;
  int port = 8080;
  std::string flags_url;
  std::optional<std::string> flags_token;
  std::chrono::seconds flags_poll{30};

  // The environment, falling back to what DOC knew when this service was created.
  static Config load();
};

}  // namespace {{ scaffold.package }}
