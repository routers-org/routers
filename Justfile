mod tz "libs/routers_tz"

init VERSION="2026a":
    just tz download {{ VERSION }}
