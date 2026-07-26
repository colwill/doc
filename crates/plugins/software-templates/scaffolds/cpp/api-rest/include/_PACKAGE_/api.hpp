// {{ values.name }}'s HTTP API: health, readiness and the service's own routes.
#pragma once

#include <httplib.h>

#include "{{ scaffold.package }}/config.hpp"
#include "{{ scaffold.package }}/flags.hpp"
#include "{{ scaffold.package }}/telemetry.hpp"

namespace {{ scaffold.package }} {

// Everything {{ values.name }} answers, ready to listen.
void routes(httplib::Server& server, const Config& config, const Flags& flags,
            const Telemetry& telemetry);

}  // namespace {{ scaffold.package }}
