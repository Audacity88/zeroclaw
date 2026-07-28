#!/usr/bin/env bash

set -euo pipefail

runs="${ZEROCLAW_PARALLEL_TEST_RUNS:-3}"
threads="${ZEROCLAW_PARALLEL_TEST_THREADS:-16}"
scope="${ZEROCLAW_PARALLEL_TEST_SCOPE:-all}"

case "$runs" in
    ''|*[!0-9]*|0)
        echo "ZEROCLAW_PARALLEL_TEST_RUNS must be a positive integer (got: $runs)."
        exit 2
        ;;
esac

case "$threads" in
    ''|*[!0-9]*|0)
        echo "ZEROCLAW_PARALLEL_TEST_THREADS must be a positive integer (got: $threads)."
        exit 2
        ;;
esac

case "$scope" in
    all)
        package_args=(-p zeroclaw-runtime -p zeroclaw-channels)
        ;;
    channels)
        package_args=(-p zeroclaw-channels)
        ;;
    *)
        echo "ZEROCLAW_PARALLEL_TEST_SCOPE must be 'channels' or 'all' (got: $scope)."
        exit 2
        ;;
esac

for ((run = 1; run <= runs; run++)); do
    echo "==> parallel runtime regression: $scope run $run/$runs ($threads threads)"
    cargo test --locked --quiet "${package_args[@]}" --lib -- --test-threads="$threads"
done
