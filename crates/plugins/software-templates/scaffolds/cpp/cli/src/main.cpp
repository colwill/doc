// {{ values.name }} — {{ values.description }}
//
// Each command is one function, and main chooses between them, so adding one is adding an entry to
// the table below.
#include <algorithm>
#include <functional>
#include <iostream>
#include <string>
#include <vector>

#include "{{ scaffold.package }}/config.hpp"
#include "{{ scaffold.package }}/flags.hpp"
#include "{{ scaffold.package }}/telemetry.hpp"

namespace {

using {{ scaffold.package }}::Config;
using {{ scaffold.package }}::Flags;
using {{ scaffold.package }}::Telemetry;

int hello(const Config& config, const Flags& flags, const std::vector<std::string>& arguments) {
  std::string who = "world";
  for (std::size_t at = 0; at + 1 < arguments.size(); ++at) {
    if (arguments[at] == "--who") who = arguments[at + 1];
  }

  // A flag read here is DOC's, with the value this service falls back to beside it.
  std::string greeting = flags.string("greeting", "Hello");
  if (flags.boolean("shout", false)) {
    std::transform(greeting.begin(), greeting.end(), greeting.begin(),
                   [](unsigned char letter) { return std::toupper(letter); });
  }
  std::cout << greeting << ", " << who << " — from " << config.service << " in "
            << config.environment << '\n';
  return 0;
}

int show_flags(const Config& config, const Flags& flags, const std::vector<std::string>&) {
  std::cout << config.service << " reads its flags from " << config.flags_url << '\n';
  std::cout << "  greeting = " << flags.string("greeting", "Hello") << '\n';
  std::cout << "  shout    = " << (flags.boolean("shout", false) ? "true" : "false") << '\n';
  if (!config.flags_token.has_value()) {
    std::cerr << "note: DOC_FLAGS_TOKEN is not set, so only public flags are read\n";
  }
  return 0;
}

struct Command {
  std::string name;
  std::string about;
  std::function<int(const Config&, const Flags&, const std::vector<std::string>&)> run;
};

std::vector<Command> commands() {
  std::vector<Command> all;
  all.push_back(Command{"hello", "Says hello, and shows what the platform is telling this service", hello});
  all.push_back(Command{"flags", "Prints every flag and setting DOC holds for this service", show_flags});
  return all;
}

void usage() {
  std::cout << "{{ values.name }} — {{ values.description }}\n\nCommands:\n";
  for (const auto& one : commands()) {
    std::cout << "  " << one.name << "\t" << one.about << '\n';
  }
}

}  // namespace

int main(int argc, char** argv) {
  const auto config = Config::load();
  const Telemetry telemetry(config);
  Flags flags(config);
  flags.start();

  const std::vector<std::string> arguments(argv + 1, argv + argc);
  if (arguments.empty() || arguments[0] == "-h" || arguments[0] == "--help") {
    usage();
    return 0;
  }
  const auto span = telemetry.tracer()->StartSpan("command");
  for (const auto& one : commands()) {
    if (one.name == arguments[0]) {
      return one.run(config, flags, std::vector<std::string>(arguments.begin() + 1, arguments.end()));
    }
  }
  std::cerr << "there is no command called " << arguments[0] << '\n';
  usage();
  return 1;
}
