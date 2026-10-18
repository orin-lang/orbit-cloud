# Appliance OS image

A minimal Debian (trixie) disk image, built with [mkosi](https://github.com/systemd/mkosi), whose only service is
`orbitd`.

| | |
|---|---|
| Contents | systemd, networkd (DHCP), resolved, `orbitd`, nothing else: no shell login, no SSH, no package manager activity |
| Partitions | ESP (256 MiB), root (ext4, **read-only**), `orbit-data` (ext4, the rest of the disk) |
| Data | `orbit-data` is mounted at `/var/lib/orbit-cloud` (`nodev,nosuid,noexec`); the database and app files live there |
| Service | `orbitd.service`: user `orbitd`, sandboxed like `deploy/systemd/orbitd.service`, listens on 8080 |
| Admin token | the systemd credential `orbit-cloud.token`; without it `orbitd` refuses to start (the API must not be open) |
| Updates | replace the image (A/B partitions are planned for milestone 2); the data partition survives if you keep the disk |

## Build

Requirements: Linux with root (mkosi uses user namespaces or `sudo`), mkosi 25 or newer, `qemu-system-x86_64` and OVMF
to boot it, a Rust toolchain.

```sh
cd deploy/os
./build.sh            # cargo build --release, stage orbitd into mkosi.extra/usr/bin, run mkosi
./build.sh -f         # rebuild an existing image
ORBITD_BIN=/path/to/orbitd ./build.sh   # use a binary you built elsewhere
```

The result is `deploy/os/mkosi.output/orbit-cloud.raw` (a sparse GPT disk image). The image is Debian trixie, so build
`orbitd` on a host with glibc 2.41 or older; the binary from `docker build --target build` (Debian bookworm, glibc
2.36) works too: pass it as `ORBITD_BIN`.

A bigger data partition: `truncate -s 64G mkosi.output/orbit-cloud.raw`, then move the backup GPT with
`sgdisk -e` and grow the last partition (`parted resizepart 3`); the file system grows at the next boot
(`x-systemd.growfs`).

## Boot in QEMU

With mkosi (forwards `127.0.0.1:18080` to port 8080, see `[Runtime]` in `mkosi.conf`):

```sh
cd deploy/os
mkosi --credential=orbit-cloud.token=secret qemu
```

or with plain QEMU:

```sh
qemu-system-x86_64 -enable-kvm -m 1G -smp 2 -nographic \
  -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
  -drive file=mkosi.output/orbit-cloud.raw,format=raw \
  -nic user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:18080-:8080 \
  -smbios type=11,value=io.systemd.credential:orbit-cloud.token=secret
```

Then, from the host:

```sh
export ORBIT_CLOUD_URL=http://127.0.0.1:18080 ORBIT_CLOUD_TOKEN=secret
curl $ORBIT_CLOUD_URL/healthz
occtl deploy hello hello.wasm && occtl run hello
```

Quit QEMU with `Ctrl-a x`. To get a recovery shell, put `RootPassword=...` in `deploy/os/mkosi.local.conf` (git-ignored)
and rebuild. On a cloud provider or bare metal, pass the credential the way the platform supports systemd credentials
(a `.cred` file on the ESP, SMBIOS OEM strings, or `systemd.set_credential=` on the kernel command line for tests).

## Status

The recipe is reviewed against the mkosi and systemd documentation but has not been built in CI: that needs root,
network access and KVM. Treat the first build as a test and report what differs.
