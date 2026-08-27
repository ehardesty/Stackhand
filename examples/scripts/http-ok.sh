#!/bin/sh

# Return one small HTTP 200 response for each connection. Stackhand uses this
# only as a local readiness example. It is not a general HTTP server.
set -eu

port="${1:-43124}"

printf 'HTTP example listening on http://127.0.0.1:%s/health\n' "$port"
while :; do
    # Apple nc accepts one connection and exits. The loop opens the listener
    # again for the next readiness attempt or manual request.
    printf 'HTTP/1.0 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n' |
        nc -l 127.0.0.1 "$port"
done
