#!/bin/bash
set -x

source $HOME/.cargo/env

# Download, build and install all the CI artifacts
source ./scripts/prepare_artifacts.sh

# Set default libc if we're not being called from dev_cli.sh
: "${CH_LIBC:="gnu"}"

# VFIO test network setup.
# We reserve a different IP class for it: 172.17.0.0/24.
sudo ip link add name vfio-br0 type bridge
sudo ip link set vfio-br0 up
sudo ip addr add 172.17.0.1/24 dev vfio-br0

sudo ip tuntap add vfio-tap0 mode tap
sudo ip link set vfio-tap0 master vfio-br0
sudo ip link set vfio-tap0 up

sudo ip tuntap add vfio-tap1 mode tap
sudo ip link set vfio-tap1 master vfio-br0
sudo ip link set vfio-tap1 up

sudo ip tuntap add vfio-tap2 mode tap
sudo ip link set vfio-tap2 master vfio-br0
sudo ip link set vfio-tap2 up

sudo ip tuntap add vfio-tap3 mode tap
sudo ip link set vfio-tap3 master vfio-br0
sudo ip link set vfio-tap3 up

# Create tap interface without multipe queues support for vhost_user_net test.
sudo ip tuntap add name vunet-tap0 mode tap
# Create tap interface with multipe queues support for vhost_user_net test.
sudo ip tuntap add name vunet-tap1 mode tap multi_queue

BUILD_TARGET="$(uname -m)-unknown-linux-${CH_LIBC}"
CFLAGS=""
TARGET_CC=""
if [[ "${BUILD_TARGET}" == "x86_64-unknown-linux-musl" ]]; then
TARGET_CC="musl-gcc"
CFLAGS="-I /usr/include/x86_64-linux-musl/ -idirafter /usr/include/"
fi

cargo build --release --target $BUILD_TARGET
strip target/$BUILD_TARGET/release/cloud-hypervisor
strip target/$BUILD_TARGET/release/vhost_user_net
strip target/$BUILD_TARGET/release/ch-remote

# Copy for non-privileged net test
cp target/$BUILD_TARGET/release/cloud-hypervisor target/$BUILD_TARGET/release/cloud-hypervisor-unprivileged

sudo setcap cap_net_admin+ep target/$BUILD_TARGET/release/cloud-hypervisor
sudo setcap cap_net_admin+ep target/$BUILD_TARGET/release/vhost_user_net

# We always copy a fresh version of our binary for our L2 guest.
cp target/$BUILD_TARGET/release/cloud-hypervisor $VFIO_DIR
cp target/$BUILD_TARGET/release/ch-remote $VFIO_DIR

# Enable KSM with some reasonable parameters so that it won't take too long
# for the memory to be merged between two processes.
sudo bash -c "echo 1000000 > /sys/kernel/mm/ksm/pages_to_scan"
sudo bash -c "echo 10 > /sys/kernel/mm/ksm/sleep_millisecs"
sudo bash -c "echo 1 > /sys/kernel/mm/ksm/run"

# Ensure test binary has the same caps as the cloud-hypervisor one
time cargo test --no-run --features "integration_tests" -- --nocapture || exit 1
ls target/debug/deps/cloud_hypervisor-* | xargs -n 1 sudo setcap cap_net_admin+ep

# test_vfio relies on hugepages
echo 4096 | sudo tee /proc/sys/vm/nr_hugepages
sudo chmod a+rwX /dev/hugepages

sudo adduser $USER kvm
newgrp kvm << EOF
export RUST_BACKTRACE=1
time cargo test --features "integration_tests" "$@"
EOF
RES=$?

if [ $RES -eq 0 ]; then
    # virtio-mmio based testing
    cargo build --release --target $BUILD_TARGET --no-default-features --features "mmio"
    strip target/$BUILD_TARGET/release/cloud-hypervisor
    strip target/$BUILD_TARGET/release/vhost_user_net
    strip target/$BUILD_TARGET/release/ch-remote

    sudo setcap cap_net_admin+ep target/$BUILD_TARGET/release/cloud-hypervisor

    # Ensure test binary has the same caps as the cloud-hypervisor one
    time cargo test --no-run --features "integration_tests,mmio" -- --nocapture || exit 1
    ls target/debug/deps/cloud_hypervisor-* | xargs -n 1 sudo setcap cap_net_admin+ep

    newgrp kvm << EOF
export RUST_BACKTRACE=1
time cargo test --features "integration_tests,mmio" "$@"
EOF

    RES=$?
fi

# Tear VFIO test network down
sudo ip link del vfio-br0
sudo ip link del vfio-tap0
sudo ip link del vfio-tap1
sudo ip link del vfio-tap2
sudo ip link del vfio-tap3

# Tear vhost_user_net test network down
sudo ip link del vunet-tap0
sudo ip link del vunet-tap1

exit $RES
