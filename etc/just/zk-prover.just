set positional-arguments := true
set working-directory := '../..'

_ca_compose := "etc/scripts/zk-prover/ca-compose.sh"
_ca_combined_file := "/tmp/zk-prover-ca-certificates.crt"
_ca_extra_file := "/etc/ssl/certs/zk-prover-extra-ca.pem"
_ca_service := "base-prover-zk-host"
_ca_service_command := "./base-prover-zk-host"
_ca_system_file := "/etc/ssl/certs/ca-certificates.crt"
_compose := "-f etc/docker/docker-compose.zk-prover.yml"
_endpoint := "${PROVER_SERVICE_ENDPOINT:-http://localhost:${PROVER_SERVICE_RPC_PORT:-9000}}"
_project := "zk-prover"
_yaml_to_env := "etc/scripts/yaml-to-compose-env.py"

# Show prover-service request commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  zk-prover::'

# Start a standalone prover-service and ZK host for a network: just zk-prover up network=sepolia mode=dry-run|cluster
up network='devnet' mode='dry-run':
    #!/usr/bin/env bash
    set -euo pipefail
    just --justfile {{ source_file() }} _up "{{ network }}" "{{ mode }}"

# Stop the active standalone prover-service stack
down:
    #!/usr/bin/env bash
    set -euo pipefail
    docker compose -p "{{ _project }}" {{ _compose }} down --remove-orphans

# Stream active standalone prover-service stack logs
logs *containers:
    #!/usr/bin/env bash
    set -euo pipefail
    docker compose -p "{{ _project }}" {{ _compose }} logs -f {{ if containers == "" { "base-prover-service base-prover-zk-host prover-service-postgres" } else { containers } }}

# List JSON-RPC methods exposed by the active prover-service endpoint
methods:
    #!/usr/bin/env bash
    set -euo pipefail
    curl -fsS \
      --header 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","method":"rpc_methods","params":[],"id":1}' \
      "{{ _endpoint }}"
    printf '\n'

# Alias for listing JSON-RPC methods
list:
    @just --justfile {{ source_file() }} methods

# Request a proof or dry-run for a block range: proof_type=compressed|snark
prove start_block number_of_blocks='1' proof_type='compressed' session_id='' prover_address='' l1_head='':
    #!/usr/bin/env bash
    set -euo pipefail
    command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }
    start_block={{ quote(start_block) }}
    start_block="${start_block#start_block=}"
    number_of_blocks={{ quote(number_of_blocks) }}
    number_of_blocks="${number_of_blocks#number_of_blocks=}"
    proof_type={{ quote(proof_type) }}
    proof_type="${proof_type#proof_type=}"
    proof_type="$(printf '%s' "$proof_type" | tr '[:upper:]-' '[:lower:]_')"
    session_id={{ quote(session_id) }}
    session_id="${session_id#session_id=}"
    prover_address={{ quote(prover_address) }}
    prover_address="${prover_address#prover_address=}"
    l1_head={{ quote(l1_head) }}
    l1_head="${l1_head#l1_head=}"

    case "$start_block" in
      ''|*[!0-9]*) echo "start_block must be a non-negative integer" >&2; exit 1 ;;
    esac
    case "$number_of_blocks" in
      ''|*[!0-9]*) echo "number_of_blocks must be a non-negative integer" >&2; exit 1 ;;
    esac

    if [ -z "$session_id" ]; then
      session_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
    fi
    if [ -z "$prover_address" ]; then
      prover_address="0x0000000000000000000000000000000000000000"
    fi

    case "$proof_type" in
      compressed)
        payload="$(jq -n -c \
          --arg session_id "$session_id" \
          --argjson start_block "$start_block" \
          --argjson number_of_blocks "$number_of_blocks" \
          --arg l1_head "$l1_head" \
          '{
            jsonrpc: "2.0",
            method: "prover_proveBlockRange",
            params: [{
              proof: {
                session_id: $session_id,
                request: {
                  proof_type: "compressed",
                  payload: ({
                    start_block_number: $start_block,
                    number_of_blocks_to_prove: $number_of_blocks,
                    zk_vm: "sp1"
                  } + if $l1_head == "" then {} else {l1_head: $l1_head} end)
                }
              }
            }],
            id: 1
          }')"
        ;;
      snark|groth16|snark_groth16)
        payload="$(jq -n -c \
          --arg session_id "$session_id" \
          --argjson start_block "$start_block" \
          --argjson number_of_blocks "$number_of_blocks" \
          --arg prover_address "$prover_address" \
          --arg l1_head "$l1_head" \
          '{
            jsonrpc: "2.0",
            method: "prover_proveBlockRange",
            params: [{
              proof: {
                session_id: $session_id,
                request: {
                  proof_type: "snark_groth16",
                  payload: {
                    proof: ({
                      start_block_number: $start_block,
                      number_of_blocks_to_prove: $number_of_blocks,
                      zk_vm: "sp1"
                    } + if $l1_head == "" then {} else {l1_head: $l1_head} end),
                    prover_address: $prover_address
                  }
                }
              }
            }],
            id: 1
          }')"
        ;;
      *)
        echo "unknown proof_type: $proof_type (use compressed or snark)" >&2
        exit 1
        ;;
    esac

    curl -fsS --header 'content-type: application/json' --data "$payload" "{{ _endpoint }}"
    printf '\n'

# Get proof status and result
get session_id:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }
    session_id={{ quote(session_id) }}
    session_id="${session_id#session_id=}"
    payload="$(jq -n -c \
      --arg session_id "$session_id" \
      '{
        jsonrpc: "2.0",
        method: "prover_getProof",
        params: [{session_id: $session_id}],
        id: 1
      }')"
    curl -fsS --header 'content-type: application/json' --data "$payload" "{{ _endpoint }}"
    printf '\n'

# Delete a terminal proof request so the same session id can be retried
delete session_id:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }
    session_id={{ quote(session_id) }}
    session_id="${session_id#session_id=}"
    payload="$(jq -n -c \
      --arg session_id "$session_id" \
      '{
        jsonrpc: "2.0",
        method: "prover_deleteProofRequest",
        params: [{session_id: $session_id}],
        id: 1
      }')"
    curl -fsS --header 'content-type: application/json' --data "$payload" "{{ _endpoint }}"
    printf '\n'

# List recent proof requests
list-proofs limit='20' offset='0' status_filter='':
    #!/usr/bin/env bash
    set -euo pipefail
    command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }
    limit={{ quote(limit) }}
    limit="${limit#limit=}"
    offset={{ quote(offset) }}
    offset="${offset#offset=}"
    status_filter={{ quote(status_filter) }}
    status_filter="${status_filter#status_filter=}"
    status_filter="$(printf '%s' "$status_filter" | tr '[:upper:]-' '[:lower:]_')"
    if [ "$status_filter" = "pending" ]; then
      status_filter="queued"
    fi
    case "$limit" in
      ''|*[!0-9]*) echo "limit must be a non-negative integer" >&2; exit 1 ;;
    esac
    case "$offset" in
      ''|*[!0-9]*) echo "offset must be a non-negative integer" >&2; exit 1 ;;
    esac
    case "$status_filter" in
      ""|queued|running|succeeded|failed) ;;
      *)
        echo "unknown status_filter: $status_filter (use queued, running, succeeded, or failed)" >&2
        exit 1
        ;;
    esac
    payload="$(jq -n -c \
      --argjson limit "$limit" \
      --argjson offset "$offset" \
      --arg status_filter "$status_filter" \
      '{
        jsonrpc: "2.0",
        method: "prover_listProofs",
        params: [({
          limit: $limit,
          offset: $offset
        } + if $status_filter == "" then {} else {status_filter: $status_filter} end)],
        id: 1
      }')"
    curl -fsS --header 'content-type: application/json' --data "$payload" "{{ _endpoint }}"
    printf '\n'

[private]
_network-env network output:
    #!/usr/bin/env bash
    set -euo pipefail
    network="{{ network }}"
    network="${network#network=}"
    config="${ZK_NETWORK_CONFIG:-${BASE_NETWORK_CONFIG_DIR:-$HOME/.config/base/networks}/${network}.yaml}"
    if [ ! -f "$config" ]; then
      echo "ERROR: network YAML not found: $config" >&2
      echo "Create it with rpc, l1_rpc, consensus_node_rpc, and l1_beacon_rpc fields." >&2
      exit 1
    fi
    python3 "{{ _yaml_to_env }}" "$config" "{{ output }}" \
      --map rpc=L2_NODE_ADDRESS \
      --map l2_rpc=L2_NODE_ADDRESS \
      --map l1_rpc=L1_NODE_ADDRESS \
      --map consensus_node_rpc=BASE_CONSENSUS_ADDRESS \
      --map op_node_rpc=BASE_CONSENSUS_ADDRESS \
      --map base_consensus_rpc=BASE_CONSENSUS_ADDRESS \
      --map l1_beacon_rpc=L1_BEACON_ADDRESS \
      --map l1_beacon=L1_BEACON_ADDRESS \
      --map beacon_rpc=L1_BEACON_ADDRESS \
      --default PROVER_SERVICE_NETWORK="$network" \
      --require L2_NODE_ADDRESS \
      --require L1_NODE_ADDRESS \
      --require BASE_CONSENSUS_ADDRESS \
      --require L1_BEACON_ADDRESS

[private]
_cluster-env network output:
    #!/usr/bin/env bash
    set -euo pipefail
    network="{{ network }}"
    network="${network#network=}"
    config="${ZK_CLUSTER_CONFIG:-${BASE_CLUSTER_CONFIG_DIR:-$HOME/.config/base/clusters}/${network}.yaml}"
    if [ ! -f "$config" ]; then
      echo "ERROR: ZK cluster YAML not found: $config" >&2
      echo "Create it with non-secret cluster settings, then retry." >&2
      exit 1
    fi
    python3 "{{ _yaml_to_env }}" "$config" "{{ output }}" \
      --map sp1.prover=ZK_BACKEND \
      --map sp1.cluster_api_endpoint=SP1_CLUSTER_API_ENDPOINT \
      --map sp1.cluster_timeout_hours=SP1_CLUSTER_TIMEOUT_HOURS \
      --map artifact.s3_bucket=CLI_S3_BUCKET \
      --map artifact.s3_region=CLI_S3_REGION \
      --map aws.endpoint_url=AWS_ENDPOINT_URL \
      --map aws.default_region=AWS_DEFAULT_REGION \
      --map prover.default_sequence_window=DEFAULT_SEQUENCE_WINDOW \
      --map tls.ca_cert_file=ZK_PROVER_CA_CERT_FILE \
      --map sp1.ca_cert_file=ZK_PROVER_CA_CERT_FILE \
      --map sp1.cluster_ca_cert_file=ZK_PROVER_CA_CERT_FILE \
      --map logging.rust_log=RUST_LOG \
      --default ZK_BACKEND=cluster \
      --default SP1_CLUSTER_TIMEOUT_HOURS=24 \
      --default CLI_S3_REGION=us-east-1 \
      --default AWS_DEFAULT_REGION=us-east-1 \
      --require SP1_CLUSTER_API_ENDPOINT \
      --require CLI_S3_BUCKET \
      --deny aws.access_key_id \
      --deny aws.secret_access_key \
      --deny aws.session_token

[private]
_build profile='release':
    #!/usr/bin/env bash
    set -euo pipefail
    case "$(uname -m)" in
      x86_64)       platform_pair="linux-amd64" ;;
      arm64|aarch64) platform_pair="linux-arm64" ;;
      *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
    esac
    ZK_PROVER_PROFILE="{{ profile }}" PLATFORM_PAIR="${platform_pair}" docker buildx bake -f etc/docker/docker-bake.hcl prover-service zk-host --load

[private]
_up network mode:
    #!/usr/bin/env bash
    set -euo pipefail
    mode="{{ mode }}"
    mode="${mode#mode=}"
    network="{{ network }}"
    network="${network#network=}"
    network_env="$(mktemp)"
    cluster_env=""
    ca_compose=""
    cleanup_temp_files() {
      rm -f "$network_env"
      if [ -n "$cluster_env" ]; then
        rm -f "$cluster_env"
      fi
      if [ -n "$ca_compose" ]; then
        rm -f "$ca_compose"
      fi
    }
    trap cleanup_temp_files EXIT
    just --justfile {{ source_file() }} _network-env "$network" "$network_env"

    case "$mode" in
      dry-run)
        just --justfile {{ source_file() }} _build "release"
        docker compose -p "{{ _project }}" --env-file "$network_env" {{ _compose }} down --remove-orphans
        ZK_BACKEND=dry_run docker compose -p "{{ _project }}" --env-file "$network_env" {{ _compose }} up -d --no-build
        ;;
      cluster)
        if [ -z "${AWS_ACCESS_KEY_ID:-}" ] || [ -z "${AWS_SECRET_ACCESS_KEY:-}" ]; then
          echo "ERROR: mode=cluster requires AWS credentials in the caller environment." >&2
          exit 1
        fi
        cluster_env="$(mktemp)"
        just --justfile {{ source_file() }} _cluster-env "$network" "$cluster_env"
        ca_cert_file="${ZK_PROVER_CA_CERT_FILE:-}"
        if [ -z "$ca_cert_file" ]; then
          ca_cert_file="$(awk -F= '$1 == "ZK_PROVER_CA_CERT_FILE" {print substr($0, index($0, "=") + 1)}' "$cluster_env")"
        fi
        compose_files=({{ _compose }})
        if [ -n "$ca_cert_file" ]; then
          if [ ! -f "$ca_cert_file" ]; then
            echo "ERROR: ZK prover CA certificate file not found: $ca_cert_file" >&2
            exit 1
          fi
          ca_compose="$(mktemp)"
          bash "{{ _ca_compose }}" "$ca_cert_file" "$ca_compose" \
            --service-name "{{ _ca_service }}" \
            --service-command "{{ _ca_service_command }}" \
            --system-ca-file "{{ _ca_system_file }}" \
            --extra-ca-file "{{ _ca_extra_file }}" \
            --combined-ca-file "{{ _ca_combined_file }}"
          compose_files+=(-f "$ca_compose")
        fi
        just --justfile {{ source_file() }} _build "release"
        docker compose -p "{{ _project }}" --env-file "$network_env" --env-file "$cluster_env" "${compose_files[@]}" down --remove-orphans
        docker compose -p "{{ _project }}" --env-file "$network_env" --env-file "$cluster_env" "${compose_files[@]}" up -d --no-build
        ;;
      *)
        echo "ERROR: mode must be 'dry-run' or 'cluster' (got '{{ mode }}')" >&2
        exit 1
        ;;
    esac
    printf 'Prover-service active network: %s\n' "$network"
    printf 'Prover-service endpoint: %s\n' "{{ _endpoint }}"
