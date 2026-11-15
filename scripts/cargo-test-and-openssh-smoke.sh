#!/bin/sh

set -eu

work_dir=$(mktemp -d)
server_log="${work_dir}/server.log"
ssh_log="${work_dir}/ssh.log"
server_pid=
target_dir=${CARGO_TARGET_DIR:-target}

show_logs() {
    echo "--- russhpit log ---" >&2
    cat "${server_log}" >&2 2>/dev/null || :
    echo "--- OpenSSH log ---" >&2
    cat "${ssh_log}" >&2 2>/dev/null || :
}

fail() {
    echo "test failed: $*" >&2
    show_logs
    exit 1
}

cleanup() {
    trap - EXIT
    if [ -n "${server_pid}" ] && kill -0 "${server_pid}" 2>/dev/null; then
        kill -TERM "${server_pid}" 2>/dev/null || :
        wait "${server_pid}" 2>/dev/null || :
    fi
    rm -rf "${work_dir}"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

echo "==> Run cargo tests"
cargo test --locked

echo "==> Build the smoke-test binary"
cargo build --locked

echo "==> Run the OpenSSH 1024-line smoke test"
"${target_dir}/debug/russhpit" -4 -v -d 5 -l 3 -p 2222 \
    >"${server_log}" 2>&1 &
server_pid=$!

ready=0
attempt=0
while [ "${attempt}" -lt 100 ]; do
    if grep -q ' Delay 5$' "${server_log}"; then
        ready=1
        break
    fi
    if ! kill -0 "${server_pid}" 2>/dev/null; then
        fail "russhpit stopped before it became ready"
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done
[ "${ready}" -eq 1 ] || fail "russhpit did not become ready"

set +e
timeout 20s ssh \
    -F /dev/null \
    -o BatchMode=yes \
    -o GlobalKnownHostsFile=/dev/null \
    -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null \
    -p 2222 \
    127.0.0.1 true \
    >"${ssh_log}" 2>&1
ssh_status=$?
set -e

[ "${ssh_status}" -ne 0 ] || fail "OpenSSH accepted the endless banner"
[ "${ssh_status}" -ne 124 ] || fail "OpenSSH did not close within 20 seconds"

kill -0 "${server_pid}" 2>/dev/null ||
    fail "russhpit stopped before the smoke test completed"
kill -TERM "${server_pid}"
wait "${server_pid}"
server_pid=

close_count=$(grep -c ' CLOSE host=' "${server_log}" || :)
[ "${close_count}" -eq 1 ] || fail "expected one CLOSE line, got ${close_count}"

close_line=$(grep ' CLOSE host=' "${server_log}")
bytes=$(printf '%s\n' "${close_line}" | awk '{
    for (i = 1; i <= NF; i++) {
        if ($i ~ /^bytes=/) {
            sub(/^bytes=/, "", $i)
            print $i
        }
    }
}')
duration=$(printf '%s\n' "${close_line}" | awk '{
    for (i = 1; i <= NF; i++) {
        if ($i ~ /^time=/) {
            sub(/^time=/, "", $i)
            print $i
        }
    }
}')

case "${bytes}" in
    ''|*[!0-9]*) fail "CLOSE has an invalid bytes field: ${bytes}" ;;
esac
[ "${bytes}" -ge 3072 ] || fail "CLOSE bytes ${bytes} is below 3072"
[ "${bytes}" -le 3120 ] || fail "CLOSE bytes ${bytes} is above 3120"

if ! awk -v value="${duration}" \
    'BEGIN { exit !(value >= 4.5 && value < 20) }'
then
    fail "CLOSE time ${duration} is outside 4.5 through 20 seconds"
fi

echo "OpenSSH closed after ${duration} seconds and ${bytes} bytes"
