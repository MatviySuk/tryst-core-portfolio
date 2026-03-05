#!/bin/bash
set -e

# This script is a convenience wrapper for running the Dockerized benchmarks.
# It maps a simple profile name to the full set of Docker environment variables.

PROFILE=$1
TIMESTAMP=$(date +%Y%m%d_%H%M%S)

# Change directory to the root of rust/core
cd "$(dirname "$0")/.."

RESULTS_DIR="bench_results"

if [ -z "$PROFILE" ]; then
    echo "Usage: ./scripts/run_docker_benchmarks.sh [baseline|lte|edge|all]"
    echo "  baseline: Simulates a high-speed, low-latency connection."
    echo "  lte:      Simulates an average mobile LTE connection."
    echo "  edge:     Simulates a degraded 3G connection with high latency and packet loss."
    echo "  all:      Runs baseline, lte, and edge profiles sequentially."
    exit 1
fi

# Create results directory if it doesn't exist
mkdir -p $RESULTS_DIR

# --- Function to run a specific profile ---
run_profile() {
    local profile_name=$1
    local delay=$2
    local loss=$3
    local rate=$4
    local current_timestamp=$(date +%Y%m%d_%H%M%S)
    local output_file="${profile_name}_${current_timestamp}.csv"

    echo ""
    echo "----------------------------------------------------"
    echo "Running '$profile_name' profile..."
    echo "----------------------------------------------------"

    docker run --rm --cap-add=NET_ADMIN \
      --dns 8.8.8.8 \
      -e DELAY="$delay" \
      -e LOSS="$loss" \
      -e RATE="$rate" \
      -e OUTPUT_FILE="$output_file" \
      -v "$(pwd)/$RESULTS_DIR:/results" \
      gossip-bench
}

# --- Main Execution ---

# Build the Docker image once at the start
echo "Building the Docker image..."
docker build -t gossip-bench -f Dockerfile.bench .
echo "Build complete."

if [ "$PROFILE" == "all" ]; then
    run_profile "baseline" "1ms" "0%" "250mbit"
    run_profile "lte" "50ms" "0%" "50mbit"
    run_profile "edge" "150ms" "2%" "2mbit"
else
    case $PROFILE in
      baseline)
        run_profile "baseline" "1ms" "0%" "250mbit"
        ;;
      lte)
        run_profile "lte" "50ms" "0%" "50mbit"
        ;;
      edge)
        run_profile "edge" "150ms" "2%" "2mbit"
        ;;
      *)
        echo "Error: Unknown profile '$PROFILE'."
        echo "Available profiles: baseline, lte, edge, all"
        exit 1
        ;;
    esac
fi

echo ""
echo "----------------------------------------------------"
echo "Benchmark run for profile '$PROFILE' complete."
echo "Results are available in the '$(pwd)/$RESULTS_DIR' directory."
echo "----------------------------------------------------"

