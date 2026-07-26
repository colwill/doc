// This service's feature flags and runtime configuration, read from DOC. Everything that applies to
// it is read in one call and kept here; the call carries an ETag, so a poll that finds nothing new
// costs a 304 and nothing else.
//
// Every read names the value to fall back to, so a service whose flags cannot be reached keeps
// running on its own defaults rather than stopping.
#pragma once

#include <atomic>
#include <mutex>
#include <string>
#include <thread>

#include <nlohmann/json.hpp>

#include "{{ scaffold.package }}/config.hpp"

namespace {{ scaffold.package }} {

class Flags {
 public:
  explicit Flags(Config config);
  ~Flags();

  Flags(const Flags&) = delete;
  Flags& operator=(const Flags&) = delete;

  // Reads them once, then keeps them up to date in the background until this is destroyed.
  void start();

  // One read. Answers whether anything changed.
  bool read();

  // A switch: on, off, or the fallback when the platform holds nothing for this service.
  bool boolean(const std::string& key, bool fallback) const;

  // A value read while the service runs, such as a message or a mode.
  std::string string(const std::string& key, const std::string& fallback) const;

  // A number read while the service runs, such as a limit or a timeout.
  double number(const std::string& key, double fallback) const;

  // A structured value, or the fallback.
  nlohmann::json json(const std::string& key, nlohmann::json fallback) const;

 private:
  nlohmann::json value(const std::string& key) const;

  Config config_;
  mutable std::mutex mutex_;
  nlohmann::json values_ = nlohmann::json::object();
  std::string version_;
  std::atomic<bool> stopping_{false};
  std::atomic<int> refresh_{30};
  std::thread polling_;
};

}  // namespace {{ scaffold.package }}
