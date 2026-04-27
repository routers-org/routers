mod tz "libs/timezone/routers_tz_build"

init VERSION="2026a":
    just tz download {{ VERSION }}
