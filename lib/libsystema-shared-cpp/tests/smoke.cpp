#include <array>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <span>
#include <string>
#include <string_view>
#include <sysa.hpp>
#include <unordered_map>
#include <vector>

namespace {

void require(bool condition, std::string_view what) {
    if (!condition) {
        std::fprintf(stderr, "smoke check failed: %.*s\n", static_cast<int>(what.size()), what.data());
        std::exit(EXIT_FAILURE);
    }
}

template <typename T> [[nodiscard]] T checked(T value, std::string_view what) {
    if (!value) {
        const std::string message = value.error().to_string();
        std::fprintf(stderr, "smoke check failed: %.*s: %s\n", static_cast<int>(what.size()), what.data(),
                     message.c_str());
        std::exit(EXIT_FAILURE);
    }
    return value;
}

void checked(sysa::Result const& result, std::string_view what) {
    require(result.has_value(), what);
}

void check_version() {
    require(sysa::abi_version() == SYSA_SHARED_ABI_VERSION, "ABI version matches the C header");
    require(!sysa::version().empty(), "runtime version is reported");
}

void check_config_map() {
    constexpr std::string_view json = R"({
    "Type": "notify",
    "ExecStart": ["/bin/true", "--flag"],
    "Enabled": true,
    "TimeoutStartSec": 90,
    "RetryLimit": -3,
    "SocketMode": "0660",
    "Nested": {"count": 2}
  })";

    auto map = checked(sysa::ConfigMap::from_json(json), "config map parse");

    auto type = checked(map->get_string("Type"), "config map Type");
    require(type->view() == "notify", "config map string value");

    auto exec = checked(map->get_strings("ExecStart"), "config map ExecStart");
    require(exec->size() == 2, "config map string list size");
    require((*exec)[0] == "/bin/true", "config map first list entry");
    require((*exec)[1] == "--flag", "config map second list entry");

    auto enabled = checked(map->get_bool("Enabled"), "config map Enabled");
    require(*enabled, "config map boolean value");

    auto timeout = checked(map->get_u32("TimeoutStartSec"), "config map TimeoutStartSec");
    require(*timeout == 90, "config map unsigned value");

    auto retry = checked(map->get_i32("RetryLimit"), "config map RetryLimit");
    require(*retry == -3, "config map signed value");

    auto mode = checked(map->get_mode("SocketMode"), "config map SocketMode");
    require(*mode == 0660, "config map octal mode");

    auto nested = checked(map->get_json("Nested"), "config map Nested");
    require(nested->text() == R"({"count":2})", "config map raw JSON value");

    auto round_trip = checked(map->to_json(), "config map serialize");
    auto reparsed = checked(sysa::ConfigMap::from_json(round_trip->text()), "config map reparse");
    auto reparsed_type = checked(reparsed->get_string("Type"), "reparsed Type");
    require(reparsed_type->view() == "notify", "config map round trip");

    auto absent = map->get_string("Missing");
    require(!absent && absent.error().status() == sysa::Status::not_found, "absent string is not found");

    auto absent_list = checked(map->get_strings("Missing"), "absent list");
    require(absent_list->empty(), "absent list is empty");

    auto absent_bool = checked(map->get_bool("Missing"), "absent bool");
    require(!*absent_bool, "absent bool reads false");

    auto negative_u32 = map->get_u32("RetryLimit");
    require(!negative_u32 && negative_u32.error().status() == sysa::Status::not_found,
            "a negative number is not an unsigned directive");

    auto fallback = checked(map->get_string_or("Missing", "fallback"), "string fallback");
    require(fallback == "fallback", "string fallback value");
    auto u32_fallback = checked(map->get_u32_or("Missing", 7), "u32 fallback");
    require(*u32_fallback == 7, "u32 fallback value");
    auto i32_fallback = checked(map->get_i32_or("Missing", -7), "i32 fallback");
    require(*i32_fallback == -7, "i32 fallback value");

    std::array<char, 128> buffer{};
    constexpr std::string_view buffer_json = R"({"Answer":42})";
    std::memcpy(buffer.data(), buffer_json.data(), buffer_json.size());
    auto buffered = checked(sysa::ConfigMap::from_json(buffer), "padded JSON buffer");
    auto answer = checked(buffered->get_u32("Answer"), "buffered Answer");
    require(*answer == 42, "padded JSON buffer ignores NUL padding");

    auto invalid = sysa::ConfigMap::from_json("{oops");
    require(!invalid, "invalid config map JSON is rejected");
    require(invalid.error().status() == sysa::Status::protocol, "invalid config map reports a protocol error");
    require(invalid.error().code() == sysa::ErrorCode::unit_config_decode_failed,
            "invalid config map reports the decode error code");
    require(!invalid.error().message().empty(), "invalid config map has a message");
}

void check_unit_ir() {
    constexpr std::string_view json = R"({
    "id": "example.service",
    "unit_type": "Service",
    "description": "Example",
    "source_format": "systemd",
    "source_path": "/etc/systemd/system/example.service",
    "owner": "worker-a",
    "aliases": ["a.service", "b.service"],
    "slice": "system.slice",
    "config": {"Type": "simple"},
    "template": {"definition": true, "parameters": ["x"]}
  })";

    auto ir = checked(sysa::UnitIR::from_json(json), "unit IR parse");

    auto id = checked(ir->id(), "unit IR id");
    require(id->view() == "example.service", "unit IR identifier");

    auto unit_type = checked(ir->unit_type(), "unit IR unit_type");
    require(unit_type->has_value(), "unit IR has a unit type");
    require((**unit_type).view() == "service", "unit IR type name");

    auto description = checked(ir->description(), "unit IR description");
    require(description->has_value(), "unit IR has a description");
    require((**description).view() == "Example", "unit IR description value");

    auto source_format = checked(ir->source_format(), "unit IR source_format");
    require(source_format->has_value(), "unit IR has a source format");
    require((**source_format).view() == "systemd", "unit IR source format");

    auto aliases = checked(ir->aliases(), "unit IR aliases");
    require(aliases->size() == 2, "unit IR alias count");
    require((*aliases)[1] == "b.service", "unit IR alias entry");

    auto slice = checked(ir->slice(), "unit IR slice");
    require(slice->has_value(), "unit IR has a slice");
    require((**slice).view() == "system.slice", "unit IR slice value");

    auto template_parameters = checked(ir->template_parameters(), "unit IR template parameters");
    require(template_parameters->size() == 1, "template parameter count");
    require((*template_parameters)[0] == "x", "template parameter value");
    require(ir->is_template(), "template definition flag");
    require(!ir->is_template_instance(), "template instance flag");

    auto template_of = checked(ir->template_of(), "unit IR template_of");
    require(!template_of->has_value(), "a definition has no parent template");

    auto config = checked(ir->config(), "unit IR config");
    require(config->has_value(), "unit IR has a config map");
    auto config_type = checked((**config).get_string("Type"), "unit IR config Type");
    require(config_type->view() == "simple", "unit IR config value");

    auto owner = checked(ir->owner(), "unit IR owner");
    require(owner->has_value(), "unit IR has an owner");
    require((**owner).view() == "worker-a", "unit IR owner value");

    checked(ir->set_owner("worker-b"), "unit IR set owner");
    auto new_owner = checked(ir->owner(), "unit IR new owner");
    require(new_owner->has_value() && (**new_owner).view() == "worker-b", "unit IR owner update");

    checked(ir->clear_owner(), "unit IR clear owner");
    auto no_owner = ir->owner();
    require(no_owner.has_value() && !*no_owner, "cleared owner reads as an absent optional");

    auto serialized = checked(ir->to_json(), "unit IR serialize");
    auto reparsed = checked(sysa::UnitIR::from_json(serialized->text()), "unit IR reparse");
    auto reparsed_id = checked(reparsed->id(), "reparsed IR id");
    require(reparsed_id->view() == "example.service", "unit IR round trip");

    auto invalid = sysa::UnitIR::from_json("{oops");
    require(!invalid, "invalid unit IR JSON is rejected");
    require(!invalid.error().message().empty(), "invalid unit IR has a message");
}

void check_unit_type() {
    const sysa::UnitType standard = sysa::UnitType::parse("timer");
    require(standard.kind() == sysa::UnitKind::timer, "standard unit kind");
    require(standard.is_standard(), "standard unit type is standard");
    require(standard.name() == "timer", "standard unit type name");

    const sysa::UnitType custom = sysa::UnitType::parse("openrc-script");
    require(custom.kind() == sysa::UnitKind::other, "custom unit kind");
    require(!custom.is_standard(), "custom unit type is not standard");
    require(custom.name() == "openrc-script", "custom unit type keeps its name");

    require(standard == sysa::UnitType{sysa::UnitKind::timer}, "equal standard types compare equal");
    require(std::string_view{"timer"} != sysa::UnitType::parse("socket").name(),
            "different standard types have different names");
    require(sysa::UnitType::parse("openrc-script") == custom, "custom types with the same name compare equal");
    require(!(custom == sysa::UnitType::parse("other-script")), "custom types with different names differ");
}

void check_unit_status() {
    auto status = checked(sysa::UnitStatus::create("example.service", "active", "running", 42, "invocation-1"),
                          "unit status create");

    require(status->unit_name() == "example.service", "status unit name");
    require(status->active_state() == "active", "status active state");
    require(status->sub_state() == "running", "status sub state");
    require(status->main_pid() == 42, "status main pid");
    require(status->invocation_id() == "invocation-1", "status invocation id");

    checked(status->set_extension("n_accepted", "12"), "status extension");
    std::unordered_map<std::string, std::string> more;
    more.emplace("n_failed", "0");
    more.emplace("result", "success");
    checked(status->set_extensions(more), "status extension map");

    require(status->extension_count() == 3, "status extension count");
    require(status->extensions().size() == 3, "status borrowed extensions");
    require(status->extension_map().size() == 3, "status owned extensions");

    auto first = status->extension(0);
    require(first.has_value(), "status first extension exists");
    require(!first->key.empty(), "status first extension key");
    require(status->extension_value(0).has_value(), "status first extension value");
    require(!status->extension(99).has_value(), "out-of-range extension is empty");

    auto encoded = checked(status->encode(), "status encode");
    require(!encoded->empty(), "encoded status is not empty");

    auto decoded = checked(sysa::UnitStatus::decode(encoded->bytes()), "status decode");
    require(decoded->unit_name() == "example.service", "decoded status name");
    require(decoded->active_state() == "active", "decoded status state");
    require(decoded->extension_count() == 3, "decoded status extensions");
    require(decoded->extension_map().at("n_accepted") == "12", "decoded status extension value");

    std::array<std::uint8_t, 3> malformed{0xFF, 0xFF, 0xFF};
    auto invalid = sysa::UnitStatus::decode(malformed);
    require(!invalid, "malformed status is rejected");
    require(invalid.error().status() == sysa::Status::protocol, "malformed status reports a protocol error");
}

void check_clients() {
    auto finder = checked(sysa::Finder::create("/tmp/sysa-smoke.sock"), "finder create");
    require(finder->native_handle() != nullptr, "finder handle");

    auto staging = checked(sysa::StagingAdmin::create("/tmp/sysa-smoke.sock"), "staging create");
    require(staging->native_handle() != nullptr, "staging handle");

    auto unitstate = checked(sysa::UnitStateAdmin::create("/tmp/sysa-smoke.sock"), "unit state create");
    require(unitstate->native_handle() != nullptr, "unit state handle");

    auto empty = unitstate->list(sysa::UnitStateAdmin::EntryHandler{});
    require(!empty && empty.error().status() == sysa::Status::invalid_argument, "empty unit-state handler is rejected");
}

void check_worker() {
    std::vector<std::string_view> unit_types{"service"};
    auto worker = checked(sysa::Worker::create("smoke", unit_types), "worker create");

    checked(worker->set_socket_path("/tmp/sysa-smoke.sock"), "worker socket path");
    checked(worker->enable_unit_define(), "worker unit define support");
    checked(worker->enable_resource_prepare(), "worker resource prepare support");

    bool shutdown_called = false;
    checked(worker->on_shutdown([&shutdown_called] { shutdown_called = true; }), "worker shutdown hook");
    checked(worker->on_reconnect([&shutdown_called] { shutdown_called = true; }), "worker reconnect hook");
    checked(worker->on_method_result([](sysa::MethodResultInfo const& info) { (void)info; }),
            "worker method result hook");
    checked(worker->on_envelope([](sysa::EnvelopeInfo const& info) -> sysa::Expected<bool> {
        (void)info;
        return false;
    }),
            "worker envelope hook");

    sysa::Controller controller;
    controller.on_status([](std::string_view name) -> sysa::Expected<sysa::UnitStatus> {
        return sysa::UnitStatus::create(name, "inactive", "dead", 0, "");
    });
    controller.on_start(
        [](std::string_view, std::span<const std::byte>, std::string_view) -> sysa::Result { return {}; });
    controller.on_stop([](std::string_view) -> sysa::Result { return {}; });
    controller.on_restart(
        [](std::string_view, std::span<const std::byte>, std::string_view) -> sysa::Result { return {}; });
    controller.on_reload([](std::string_view, std::span<const std::byte>) -> sysa::Result { return {}; });
    controller.on_sync_state(
        []() -> sysa::Expected<std::vector<sysa::UnitStatus>> { return std::vector<sysa::UnitStatus>{}; });
    checked(worker->set_controller(std::move(controller)), "worker controller");

    auto duplicate = worker->set_controller(sysa::Controller{});
    require(!duplicate && duplicate.error().status() == sysa::Status::internal, "a second controller is rejected");

    require(worker->publisher().native_handle() == worker->native_handle(), "publisher borrows the worker handle");
    require(worker->shutdown_handle().native_handle() == worker->native_handle(),
            "shutdown handle borrows the worker handle");

    sysa::MethodResultInfo success_result;
    success_result.success = true;
    require(success_result.failure_kind() == sysa::FailureKind::task, "a plain failure is a task fault");
    sysa::MethodResultInfo unit_result;
    unit_result.unit_failure = true;
    require(unit_result.failure_kind() == sysa::FailureKind::unit, "a unit failure is reported as a unit fault");

    checked(worker->stop(), "worker stop before start");
    auto not_started = worker->wait();
    require(!not_started && not_started.error().status() == sysa::Status::invalid_argument,
            "waiting for a worker that never started fails");
}

} // namespace

int main() {
    check_version();
    check_config_map();
    check_unit_ir();
    check_unit_type();
    check_unit_status();
    check_clients();
    check_worker();
    std::puts("sysa smoke test passed");
    return EXIT_SUCCESS;
}
