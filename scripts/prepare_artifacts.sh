export BUILD_TARGET=${BUILD_TARGET-x86_64-unknown-linux-gnu}

WORKLOADS_DIR="$HOME/workloads"
mkdir -p "$WORKLOADS_DIR"

cp scripts/sha1sums $WORKLOADS_DIR

FW="$WORKLOADS_DIR/hypervisor-fw"
if [ ! -f "$FW" ]; then
    pushd $WORKLOADS_DIR
    FW_URL=$(curl --silent https://api.github.com/repos/cloud-hypervisor/rust-hypervisor-firmware/releases/latest | grep "browser_download_url" | grep -o 'https://.*[^ "]')
    time wget --quiet $FW_URL || exit 1
    popd
fi

CLEAR_OS_IMAGE_NAME="clear-31311-cloudguest.img"
CLEAR_OS_IMAGE_URL="https://cloudhypervisorstorage.blob.core.windows.net/images/$CLEAR_OS_IMAGE_NAME"
CLEAR_OS_IMAGE="$WORKLOADS_DIR/$CLEAR_OS_IMAGE_NAME"
if [ ! -f "$CLEAR_OS_IMAGE" ]; then
    pushd $WORKLOADS_DIR
    time wget --quiet $CLEAR_OS_IMAGE_URL || exit 1
    popd
fi

CLEAR_OS_RAW_IMAGE_NAME="clear-31311-cloudguest-raw.img"
CLEAR_OS_RAW_IMAGE="$WORKLOADS_DIR/$CLEAR_OS_RAW_IMAGE_NAME"
if [ ! -f "$CLEAR_OS_RAW_IMAGE" ]; then
    pushd $WORKLOADS_DIR
    time qemu-img convert -p -f qcow2 -O raw $CLEAR_OS_IMAGE_NAME $CLEAR_OS_RAW_IMAGE_NAME || exit 1
    popd
fi

BIONIC_OS_IMAGE_NAME="bionic-server-cloudimg-amd64.img"
BIONIC_OS_IMAGE_URL="https://cloudhypervisorstorage.blob.core.windows.net/images/$BIONIC_OS_IMAGE_NAME"
BIONIC_OS_IMAGE="$WORKLOADS_DIR/$BIONIC_OS_IMAGE_NAME"
if [ ! -f "$BIONIC_OS_IMAGE" ]; then
    pushd $WORKLOADS_DIR
    time wget --quiet $BIONIC_OS_IMAGE_URL || exit 1
    popd
fi

BIONIC_OS_RAW_IMAGE_NAME="bionic-server-cloudimg-amd64-raw.img"
BIONIC_OS_RAW_IMAGE="$WORKLOADS_DIR/$BIONIC_OS_RAW_IMAGE_NAME"
if [ ! -f "$BIONIC_OS_RAW_IMAGE" ]; then
    pushd $WORKLOADS_DIR
    time qemu-img convert -p -f qcow2 -O raw $BIONIC_OS_IMAGE_NAME $BIONIC_OS_RAW_IMAGE_NAME || exit 1
    popd
fi


FOCAL_OS_IMAGE_NAME="focal-server-cloudimg-amd64.img"
FOCAL_OS_IMAGE_URL="https://cloudhypervisorstorage.blob.core.windows.net/images/$FOCAL_OS_IMAGE_NAME"
FOCAL_OS_IMAGE="$WORKLOADS_DIR/$FOCAL_OS_IMAGE_NAME"
if [ ! -f "$FOCAL_OS_IMAGE" ]; then
    pushd $WORKLOADS_DIR
    time wget --quiet $FOCAL_OS_IMAGE_URL || exit 1
    popd
fi

FOCAL_OS_RAW_IMAGE_NAME="focal-server-cloudimg-amd64-raw.img"
FOCAL_OS_RAW_IMAGE="$WORKLOADS_DIR/$FOCAL_OS_RAW_IMAGE_NAME"
if [ ! -f "$FOCAL_OS_RAW_IMAGE" ]; then
    pushd $WORKLOADS_DIR
    time qemu-img convert -p -f qcow2 -O raw $FOCAL_OS_IMAGE_NAME $FOCAL_OS_RAW_IMAGE_NAME || exit 1
    popd
fi

ALPINE_MINIROOTFS_URL="http://dl-cdn.alpinelinux.org/alpine/v3.11/releases/x86_64/alpine-minirootfs-3.11.3-x86_64.tar.gz"
ALPINE_MINIROOTFS_TARBALL="$WORKLOADS_DIR/alpine-minirootfs-x86_64.tar.gz"
if [ ! -f "$ALPINE_MINIROOTFS_TARBALL" ]; then
    pushd $WORKLOADS_DIR
    time wget --quiet $ALPINE_MINIROOTFS_URL -O $ALPINE_MINIROOTFS_TARBALL || exit 1
    popd
fi

ALPINE_INITRAMFS_IMAGE="$WORKLOADS_DIR/alpine_initramfs.img"
if [ ! -f "$ALPINE_INITRAMFS_IMAGE" ]; then
    pushd $WORKLOADS_DIR
    mkdir alpine-minirootfs
    tar xf "$ALPINE_MINIROOTFS_TARBALL" -C alpine-minirootfs
    cat > alpine-minirootfs/init <<-EOF
		#! /bin/sh
		mount -t devtmpfs dev /dev
		echo \$TEST_STRING > /dev/console
		poweroff -f
	EOF
    chmod +x alpine-minirootfs/init
    cd alpine-minirootfs
    find . -print0 |
        cpio --null --create --verbose --owner root:root --format=newc > "$ALPINE_INITRAMFS_IMAGE"
    popd
fi

pushd $WORKLOADS_DIR
sha1sum sha1sums --check
if [ $? -ne 0 ]; then
    echo "sha1sum validation of images failed, remove invalid images to fix the issue."
    exit 1
fi
popd

# Build custom kernel based on virtio-pmem and virtio-fs upstream patches
VMLINUX_IMAGE="$WORKLOADS_DIR/vmlinux"
VMLINUX_PVH_IMAGE="$WORKLOADS_DIR/vmlinux.pvh"
BZIMAGE_IMAGE="$WORKLOADS_DIR/bzImage"

LINUX_CUSTOM_DIR="$WORKLOADS_DIR/linux-custom"

if [ ! -f "$VMLINUX_IMAGE" ] || [ ! -f "$VMLINUX_PVH_IMAGE" ]; then
    SRCDIR=$PWD
    pushd $WORKLOADS_DIR
    time git clone --depth 1 "https://github.com/cloud-hypervisor/linux.git" -b "virtio-fs-virtio-iommu-virtio-mem-5.6-rc4" $LINUX_CUSTOM_DIR
    cp $SRCDIR/resources/linux-config $LINUX_CUSTOM_DIR/.config
    popd
fi

if [ ! -f "$VMLINUX_IMAGE" ]; then
    pushd $LINUX_CUSTOM_DIR
    scripts/config --disable "CONFIG_PVH"
    time make bzImage -j `nproc`
    cp vmlinux $VMLINUX_IMAGE || exit 1
    cp arch/x86/boot/bzImage $BZIMAGE_IMAGE || exit 1
    popd
fi

if [ ! -f "$VMLINUX_PVH_IMAGE" ]; then
    pushd $LINUX_CUSTOM_DIR
    scripts/config --enable "CONFIG_PVH"
    time make bzImage -j `nproc`
    cp vmlinux $VMLINUX_PVH_IMAGE || exit 1
    popd
fi

if [ -d "$LINUX_CUSTOM_DIR" ]; then
    rm -rf $LINUX_CUSTOM_DIR
fi

VIRTIOFSD="$WORKLOADS_DIR/virtiofsd"
QEMU_DIR="qemu_build"
if [ ! -f "$VIRTIOFSD" ]; then
    pushd $WORKLOADS_DIR
    git clone --depth 1 "https://github.com/sboeuf/qemu.git" -b "virtio-fs" $QEMU_DIR
    pushd $QEMU_DIR
    time ./configure --prefix=$PWD --target-list=x86_64-softmmu
    time make virtiofsd -j `nproc`
    cp virtiofsd $VIRTIOFSD || exit 1
    popd
    rm -rf $QEMU_DIR
    sudo setcap cap_dac_override,cap_sys_admin+epi "virtiofsd" || exit 1
    popd
fi

BLK_IMAGE="$WORKLOADS_DIR/blk.img"
MNT_DIR="mount_image"
if [ ! -f "$BLK_IMAGE" ]; then
   pushd $WORKLOADS_DIR
   fallocate -l 16M $BLK_IMAGE
   mkfs.ext4 -j $BLK_IMAGE
   mkdir $MNT_DIR
   sudo mount -t ext4 $BLK_IMAGE $MNT_DIR
   sudo bash -c "echo bar > $MNT_DIR/foo" || exit 1
   sudo umount $BLK_IMAGE
   rm -r $MNT_DIR
   popd
fi

SHARED_DIR="$WORKLOADS_DIR/shared_dir"
if [ ! -d "$SHARED_DIR" ]; then
    mkdir -p $SHARED_DIR
    echo "foo" > "$SHARED_DIR/file1"
    echo "bar" > "$SHARED_DIR/file3" || exit 1
fi

VFIO_DIR="$WORKLOADS_DIR/vfio"
rm -rf $VFIO_DIR
mkdir -p $VFIO_DIR
cp $CLEAR_OS_IMAGE $VFIO_DIR
cp $FW $VFIO_DIR
cp $VMLINUX_IMAGE $VFIO_DIR || exit 1