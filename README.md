# vhost-user-vsock

vhost-user-vsock daemon based on Cloud Hypervisor crates.

vhost-user-vsock provides the virtio-vsock device emulation in user-space.
The application implements the [Firecracker hybrid vsock (vsock over unix domain socket)](https://github.com/firecracker-microvm/firecracker/blob/master/docs/vsock.md)

## Build

``` shell
git clone https://github.com/stefano-garzarella/vhost-user-vsock

cd vhost-user-vsock

cargo build --release
```

## QEMU example

QEMU v5.1 will be the first release that provides vhost-user-vsock device.

```shell
# start vhost-user-vsock application
./target/release/vhost_user_vsock \
    --vsock-backend guest_cid=4,uds_path=/tmp/vm4.vsock,socket=/tmp/vhost4.socket

# start QEMU
qemu-system-x86_64 -cpu host -M q35,accel=kvm -smp 2 -m 512M \
    -object memory-backend-file,id=mem,size="512M",mem-path="/dev/hugepages",share=on \
    -numa node,memdev=mem \
    -drive file=/path/to/disk.qcow2,format=qcow2,if=none,id=hd0 \
    -device virtio-blk-pci,drive=hd0 \
    -chardev socket,id=char0,reconnect=0,path=/tmp/vhost4.socket \
    -device vhost-user-vsock-pci,chardev=char0

# run iperf-vsock between guest and host
# https://github.com/stefano-garzarella/iperf-vsock
guest$ iperf3 --vsock -s

host$ iperf3 --vsock -c /tmp/vm4.vsock
```
