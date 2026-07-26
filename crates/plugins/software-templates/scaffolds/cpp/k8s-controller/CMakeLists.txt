cmake_minimum_required(VERSION 3.24)
project({{ scaffold.package }} VERSION 0.1.0 LANGUAGES CXX DESCRIPTION "{{ values.description }}")

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

include(FetchContent)

FetchContent_Declare(nlohmann_json
  GIT_REPOSITORY https://github.com/nlohmann/json.git
  GIT_TAG v3.11.3)

set(CPR_USE_SYSTEM_CURL ON CACHE BOOL "" FORCE)
FetchContent_Declare(cpr
  GIT_REPOSITORY https://github.com/libcpr/cpr.git
  GIT_TAG 1.11.1)

set(WITH_OTLP_GRPC ON CACHE BOOL "" FORCE)
set(BUILD_TESTING OFF CACHE BOOL "" FORCE)
FetchContent_Declare(opentelemetry-cpp
  GIT_REPOSITORY https://github.com/open-telemetry/opentelemetry-cpp.git
  GIT_TAG v1.19.0)

FetchContent_MakeAvailable(nlohmann_json cpr opentelemetry-cpp)

add_library({{ scaffold.package }}_platform
  src/config.cpp
  src/telemetry.cpp
  src/flags.cpp
  src/cluster.cpp
  src/reconciler.cpp)
target_include_directories({{ scaffold.package }}_platform PUBLIC include)
target_link_libraries({{ scaffold.package }}_platform
  PUBLIC nlohmann_json::nlohmann_json cpr::cpr
         opentelemetry-cpp::trace opentelemetry-cpp::metrics
         opentelemetry-cpp::otlp_grpc_exporter opentelemetry-cpp::otlp_grpc_metrics_exporter)

add_executable({{ values.name }} src/main.cpp)
target_link_libraries({{ values.name }} PRIVATE {{ scaffold.package }}_platform)

include(CTest)
