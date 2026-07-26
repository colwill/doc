#include "{{ scaffold.package }}/config.hpp"

#include <cstdlib>
#include <string>

namespace {{ scaffold.package }} {
namespace {

std::string text(const char* name, const std::string& fallback) {
  const char* value = std::getenv(name);
  return (value != nullptr && *value != '\0') ? std::string(value) : fallback;
}

int number(const char* name, int fallback) {
  const char* value = std::getenv(name);
  if (value == nullptr || *value == '\0') return fallback;
  try {
    return std::stoi(value);
  } catch (const std::exception&) {
    return fallback;
  }
}

}  // namespace

Config Config::load() {
  Config config;
  config.service = text("OTEL_SERVICE_NAME", "{{ values.name }}");
  config.environment = text("DOC_ENVIRONMENT", "{{ telemetry.environment }}");
  config.address = text("ADDRESS", "0.0.0.0");
  config.port = number("PORT", 8080);
  config.flags_url = text("DOC_FLAGS_URL", "{{ flags.url }}");
  const std::string token = text("DOC_FLAGS_TOKEN", "");
  if (!token.empty()) config.flags_token = token;
  config.flags_poll = std::chrono::seconds(number("DOC_FLAGS_POLL_SECONDS", 30));
  return config;
}

}  // namespace {{ scaffold.package }}
