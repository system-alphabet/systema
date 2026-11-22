-- Build the Rust C ABI with cargo, then expose the header-only C++ wrapper.
--
-- The Cargo target directory lives under this project's build directory so a
-- standalone xmake build never touches the repository's top-level `target/`.

add_rules("mode.debug", "mode.release")

set_languages("c++26")
set_warnings("all", "extra")
add_cxflags("-Wpedantic", "-Werror")

local project_dir = path.absolute(os.projectdir())
local repo_dir = path.absolute(path.join(project_dir, "..", ".."))
local capi_dir = path.join(repo_dir, "lib", "libsystema-shared-capi")
local cargo_target_dir = path.join(project_dir, "build", "cargo")

local function cargo_profile()
    return is_mode("debug") and "debug" or "release"
end

local function cargo_lib_dir()
    return path.join(cargo_target_dir, cargo_profile())
end

target("libsystema-shared-capi")
    set_kind("phony")
    set_default(false)
    on_build(function (_)
        local args = {"build", "-p", "libsystema-shared-capi"}
        if not is_mode("debug") then
            table.insert(args, "--release")
        end
        os.execv("cargo", args, {
            curdir = repo_dir,
            envs = {CARGO_TARGET_DIR = cargo_target_dir}
        })
    end)
    on_clean(function (_)
        os.rm(cargo_lib_dir())
    end)

target("libsystema-shared-cpp")
    set_kind("headeronly")
    add_headerfiles("include/sysa.hpp", "include/(sysa/**.hpp)")
    add_includedirs("include", {public = true})
    add_includedirs(path.join(capi_dir, "include"), {public = true})
    add_deps("libsystema-shared-capi", {order = true})
    add_links("systema_shared_capi", {public = true})
    add_linkdirs(cargo_lib_dir(), {public = true})
    add_rpathdirs(cargo_lib_dir(), {public = true})

target("sysa-smoke")
    set_kind("binary")
    add_files("tests/smoke.cpp")
    add_deps("libsystema-shared-cpp")
    add_deps("libsystema-shared-capi", {order = true})
