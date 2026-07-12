# examples/import_fastapi.py — P0 DoD #2.
#
# Goal: prove that a non-trivial third-party module can be imported inside
# the guest. The real import chain is:
#
#   fastapi
#     -> starlette
#        -> anyio
#           -> sniffio
#           -> idna
#        -> typing_extensions
#     -> pydantic
#        -> typing_extensions
#        -> annotated_types
#
# That tree is many MB of bytecode once compiled and would need every
# extension module (anyio._backends._asyncio, ...) cross-compiled too. For
# P0 we attempt the real import; if it fails because some extension in the
# chain didn't cross-compile, the script falls back to a stdlib-only path
# that still proves the VFS / module-search-path machinery works.
#
# Stdlib path: import json, http.server, urllib.request, asyncio, typing —
# covers the same VFS, parsing, dynamic-loading, and reentrant-import code
# paths without needing fastapi's extensions.

import sys

try:
    import fastapi                       # noqa: F401
    print("fastapi-" + fastapi.__version__)
except Exception as exc:
    # Fallback per user-confirmed decision #6: real fastapi preferred;
    # stdlib fallback if cross-compile fails.
    import json
    import http.server                   # noqa: F401
    import urllib.request                # noqa: F401
    import asyncio                       # noqa: F401
    import typing                        # noqa: F401
    print(f"stdlib-ok (fastapi unavailable: {type(exc).__name__})")