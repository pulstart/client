#!/usr/bin/env bash
# Hole punching through real kernel NATs, end to end and unprivileged
# (user + network namespaces, nftables masquerade, optional netem).
#
#   internet 10.0.0.0/24: API server + STUN responder on 10.0.0.1
#     |- router 10.0.0.2 -- 192.168.1.0/24 -- st-server (synthetic capture)
#     `- router 10.0.0.3 -- 192.168.2.0/24 -- client/core test
#
# Knobs (environment):
#   NAT_SERVER, NAT_CLIENT  cone (default) | symmetric | fullcone
#   NETEM                   netem arguments for each router's WAN egress,
#                           e.g. "delay 40ms 10ms loss 2%"
#   RELAY                   up (default) | down (relay port silently dropped)
#   REBIND_AFTER            seconds until the client's public address changes
#   EXPECT                  punch (default) | relay
#   ROUNDS, HOLD_SECS, CONNECT_SECS   see client/core/tests/punch_nat_e2e.rs
#   SERVER_ENV              extra "K=V ..." for st-server
#   KEEP_LOGS=1             keep and print the log directory
set -euo pipefail

client_root=$(cd "$(dirname "$0")/.." && pwd)
server_root=${ST_SERVER_ROOT:-$client_root/../server}
api_root=${ST_API_ROOT:-$client_root/../api-server}

if [ -z "${ST_NAT_E2E_INNER:-}" ]; then
    (cd "$server_root" && cargo build)
    (cd "$api_root" && cargo build)
    (cd "$client_root/core" && cargo build --example stun_echo &&
        cargo test --no-run --test punch_nat_e2e)
    exec unshare -Urn env ST_NAT_E2E_INNER=1 "$0" "$@"
fi

work=$(mktemp -d)
token=nat-e2e-token
stun=10.0.0.1:3478,10.0.0.1:3479
declare -A ns
pids=()

cleanup() {
    status=$?
    kill "${pids[@]}" 2>/dev/null || true
    wait 2>/dev/null || true
    if [ -n "${KEEP_LOGS:-}" ] || [ "$status" -ne 0 ]; then
        echo "logs: $work"
    else
        rm -rf "$work"
    fi
}
trap cleanup EXIT

spawn_ns() {
    unshare -n sleep infinity &
    ns[$1]=$!
    pids+=($!)
    until [ "$(readlink "/proc/${ns[$1]}/ns/net")" != "$(readlink /proc/$$/ns/net)" ]; do
        sleep 0.01
    done
}

at() {
    local name=$1
    shift
    nsenter -t "${ns[$name]}" -n "$@"
}

# One NATed site: a router with a WAN leg on the internet bridge and a host
# behind it. `symmetric` picks a random source port per destination.
site() {
    local router=$1 host=$2 wan_ip=$3 lan=$4 kind=$5 flags=""
    [ "$kind" = symmetric ] && flags=random
    spawn_ns "$router"
    spawn_ns "$host"
    ip link add "up-$router" type veth peer name wan netns "${ns[$router]}"
    ip link set "up-$router" master inet0 up
    at "$router" ip link add lan type veth peer name eth0 netns "${ns[$host]}"
    at "$router" ip link set lo up
    at "$router" ip addr add "$wan_ip/24" dev wan
    at "$router" ip link set wan up
    at "$router" ip addr add "$lan.1/24" dev lan
    at "$router" ip link set lan up
    at "$router" sysctl -qw net.ipv4.ip_forward=1
    at "$host" ip link set lo up
    at "$host" ip addr add "$lan.2/24" dev eth0
    at "$host" ip link set eth0 up
    at "$host" ip route add default via "$lan.1"
    if [ "$kind" = fullcone ]; then
        # A router with a port mapping for the host (PCP/NAT-PMP, DMZ):
        # anyone may reach its UDP ports.
        at "$router" nft -f - <<EOF
table ip nat {
    chain pre { type nat hook prerouting priority -100; iifname "wan" meta l4proto udp dnat to $lan.2; }
    chain post { type nat hook postrouting priority 100; oifname "wan" masquerade; }
}
EOF
    else
        # A consumer router: masquerade outbound, drop unsolicited inbound.
        at "$router" nft -f - <<EOF
table ip nat {
    chain post { type nat hook postrouting priority 100; oifname "wan" masquerade $flags; }
}
table ip filter {
    chain input { type filter hook input priority 0; iifname "wan" ct state new drop; }
    chain forward { type filter hook forward priority 0; iifname "wan" ct state new drop; }
}
EOF
    fi
    if [ -n "${NETEM:-}" ]; then
        # shellcheck disable=SC2086
        at "$router" tc qdisc add dev wan root netem $NETEM
    fi
}

ip link set lo up
ip link add inet0 type bridge
ip addr add 10.0.0.1/24 dev inet0
ip link set inet0 up
site rs hs 10.0.0.2 192.168.1 "${NAT_SERVER:-cone}"
site rc hc 10.0.0.3 192.168.2 "${NAT_CLIENT:-cone}"
if [ "${RELAY:-up}" = down ]; then
    nft -f - <<EOF
table ip filter {
    chain input { type filter hook input priority 0; tcp dport 3001 drop; }
}
EOF
fi

BIND_ADDR=10.0.0.1:3000 RELAY_BIND_ADDR=10.0.0.1:3001 \
    "$api_root/target/debug/st-api-server" >"$work/api.log" 2>&1 &
pids+=($!)
"$client_root/core/target/debug/examples/stun_echo" 10.0.0.1:3478 10.0.0.1:3479 &
pids+=($!)

# shellcheck disable=SC2086
at hs env ST_CAPTURE=synthetic ST_SYNTHETIC_OUTPUTS=2 ST_PORT=8080 ST_TOKEN=$token \
    ST_STATE_DIR="$work/state" ST_SERVER_NO_TRAY=1 ST_WAKE_ON_CONNECT=0 \
    ST_API_URL=http://10.0.0.1:3000 ST_STUN_SERVERS=$stun ST_FPS=60 ${SERVER_ENV:-} \
    "$server_root/target/debug/st-server" >"$work/server.log" 2>&1 &
pids+=($!)

if [ -n "${REBIND_AFTER:-}" ]; then
    # The client's public address changes mid-session (handover, NAT rebind).
    at rc sysctl -qw net.ipv4.conf.all.promote_secondaries=1
    (
        sleep "$REBIND_AFTER"
        at rc ip addr add 10.0.0.4/24 dev wan
        at rc ip addr del 10.0.0.3/24 dev wan
        echo "client rebound to 10.0.0.4"
    ) &
    pids+=($!)
fi

cd "$client_root/core"
at hc env ST_NAT_E2E_API=http://10.0.0.1:3000 ST_NAT_E2E_TOKEN=$token \
    ST_NAT_E2E_SERVER_LOG="$work/server.log" ST_STUN_SERVERS=$stun \
    ST_NAT_E2E_EXPECT="${EXPECT:-punch}" ST_NAT_E2E_ROUNDS="${ROUNDS:-3}" \
    ST_NAT_E2E_HOLD_SECS="${HOLD_SECS:-20}" ST_NAT_E2E_CONNECT_SECS="${CONNECT_SECS:-30}" \
    cargo test --offline --test punch_nat_e2e -- --nocapture
