#include "{{ scaffold.package }}/flags.hpp"

#include <chrono>
#include <iostream>
#include <utility>

#include <cpr/cpr.h>

namespace {{ scaffold.package }} {

Flags::Flags(Config config) : config_(std::move(config)) {
  refresh_ = static_cast<int>(config_.flags_poll.count());
}

Flags::~Flags() {
  stopping_ = true;
  if (polling_.joinable()) polling_.join();
}

void Flags::start() {
  read();
  polling_ = std::thread([this] {
    while (!stopping_) {
      for (int waited = 0; waited < refresh_ && !stopping_; ++waited) {
        std::this_thread::sleep_for(std::chrono::seconds(1));
      }
      if (!stopping_) read();
    }
  });
}

bool Flags::read() {
  cpr::Header headers;
  headers["accept"] = "application/json";
  {
    std::lock_guard<std::mutex> holding(mutex_);
    if (!version_.empty()) headers["if-none-match"] = "\"" + version_ + "\"";
  }
  if (config_.flags_token.has_value()) {
    headers["authorization"] = "Bearer " + config_.flags_token.value();
  }

  cpr::Parameters parameters;
  parameters.Add(cpr::Parameter("service", config_.service));
  parameters.Add(cpr::Parameter("environment", config_.environment));

  const cpr::Response answer =
      cpr::Get(cpr::Url(config_.flags_url), headers, parameters, cpr::Timeout(5000));

  if (answer.status_code == 304) return false;
  if (answer.status_code != 200) {
    // The service keeps running on its own defaults rather than stopping.
    std::cerr << "the flags could not be read: " << answer.status_code << " " << answer.error.message
              << '\n';
    return false;
  }

  try {
    const auto read = nlohmann::json::parse(answer.text);
    nlohmann::json values = nlohmann::json::object();
    for (const auto& part : {std::string("flags"), std::string("config")}) {
      if (read.contains(part) && read.at(part).is_object()) {
        for (const auto& [key, value] : read.at(part).items()) values[key] = value;
      }
    }
    std::lock_guard<std::mutex> holding(mutex_);
    values_ = std::move(values);
    version_ = read.value("version", "");
    refresh_ = read.value("refresh_seconds", static_cast<int>(config_.flags_poll.count()));
    return true;
  } catch (const std::exception& problem) {
    std::cerr << "the flags could not be read: " << problem.what() << '\n';
    return false;
  }
}

nlohmann::json Flags::value(const std::string& key) const {
  std::lock_guard<std::mutex> holding(mutex_);
  const auto held = values_.find(key);
  return held == values_.end() ? nlohmann::json() : *held;
}

bool Flags::boolean(const std::string& key, bool fallback) const {
  const auto held = value(key);
  return held.is_boolean() ? held.get<bool>() : fallback;
}

std::string Flags::string(const std::string& key, const std::string& fallback) const {
  const auto held = value(key);
  return held.is_string() ? held.get<std::string>() : fallback;
}

double Flags::number(const std::string& key, double fallback) const {
  const auto held = value(key);
  return held.is_number() ? held.get<double>() : fallback;
}

nlohmann::json Flags::json(const std::string& key, nlohmann::json fallback) const {
  const auto held = value(key);
  return held.is_null() ? std::move(fallback) : held;
}

}  // namespace {{ scaffold.package }}
