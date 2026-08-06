cmake_minimum_required(VERSION 3.20)
project(Harkness VERSION 0.1.0 LANGUAGES NONE)

set(CMAKE_INSTALL_BINDIR "bin" CACHE PATH "User executable directory")
set(CMAKE_INSTALL_DATADIR "share" CACHE PATH "Read-only architecture-independent data directory")

find_program(CARGO_EXECUTABLE cargo REQUIRED)
set(HARKNESS_CARGO_TARGET_DIR "${CMAKE_CURRENT_BINARY_DIR}/cargo-target")

add_custom_target(harkness ALL
    COMMAND "${CMAKE_COMMAND}" -E env
            "CARGO_TARGET_DIR=${HARKNESS_CARGO_TARGET_DIR}"
            "${CARGO_EXECUTABLE}" build --workspace --release --locked
    WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}"
    COMMENT "Building Harkness with Cargo"
    VERBATIM
)

install(PROGRAMS
    "${HARKNESS_CARGO_TARGET_DIR}/release/harkness"
    "${HARKNESS_CARGO_TARGET_DIR}/release/harkness-gui"
    TYPE BIN
)
install(FILES data/io.github.fullstacktaiye.harkness.desktop
    DESTINATION "${CMAKE_INSTALL_DATADIR}/applications"
)
