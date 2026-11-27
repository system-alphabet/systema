-- Build the Rust C ABI with cargo, then expose the header-only C++ wrapper.
--
-- The C ABI crate is declared as a project-local xmake package: `add_requires`
-- runs cargo to build `libsystema-shared-c` and installs the shared library
-- plus the generated C header, and `add_packages` propagates the include
-- directory, the link directory and the shared-library rpath to every
-- consumer.
--
-- The built-in `cargo::` package manager cannot be used for this crate: it only
-- installs crates from a registry and appends its own `[lib]` table to the
-- manifest, which collides with the crate's existing `[lib]` section.
--
-- The Cargo target directory lives under this project's build directory so a
-- standalone xmake build never touches the repository's top-level `target/`.

add_rules("mode.debug", "mode.release")

set_languages("c++26")
set_warnings("all", "extra")
add_cxflags("-Wpedantic", "-Werror")

-- Keep the installed C ABI inside this project's build directory.
set_policy("package.install_locally", true)

local project_dir = path.absolute(os.projectdir())
local repo_dir = path.absolute(path.join(project_dir, "..", ".."))
local c_dir = path.join(repo_dir, "lib", "libsystema-shared-c")
local cargo_target_dir = path.join(project_dir, "build", "cargo")

-- The shared library name cargo produces for the current platform.
local function c_libname()
    if is_plat("windows") then
        return "systema_shared_c.dll"
    elseif is_plat("macosx") then
        return "libsystema_shared_c.dylib"
    end
    return "libsystema_shared_c.so"
end

package("libsystema-shared-c")
    set_description("C ABI of the systema shared Rust crates.")
    -- Reinstall on every require pass so Rust edits are never served from a
    -- stale package directory.
    set_policy("package.install_always", true)
    add_configs("profile", {description = "Cargo profile to build.", default = "release", values = {"debug", "release"}})

    on_install(function (package)
        local profile = package:config("profile")
        local args = {"build", "-p", "libsystema-shared-c"}
        if profile ~= "debug" then
            table.insert(args, "--release")
        end
        os.execv("cargo", args, {
            curdir = repo_dir,
            envs = {CARGO_TARGET_DIR = cargo_target_dir}
        })

        local libdir = package:installdir("lib")
        local includedir = package:installdir("include")
        os.mkdir(libdir)
        os.mkdir(includedir)
        os.cp(path.join(cargo_target_dir, profile, c_libname()), libdir)
        os.cp(path.join(c_dir, "include", "systema_shared.h"), includedir)
        package:add("links", "systema_shared_c")
        package:add("rpathdirs", path.unix(libdir))
    end)
package_end()

add_requires("libsystema-shared-c", {
    system = false,
    configs = {profile = is_mode("debug") and "debug" or "release"}
})

target("libsystema-shared-cpp")
    set_kind("headeronly")
    add_headerfiles("include/sysa.hpp", "include/(sysa/**.hpp)")
    add_includedirs("include", {public = true})
    add_packages("libsystema-shared-c", {public = true})

target("sysa-smoke")
    set_kind("binary")
    add_files("tests/smoke.cpp")
    add_deps("libsystema-shared-cpp")
