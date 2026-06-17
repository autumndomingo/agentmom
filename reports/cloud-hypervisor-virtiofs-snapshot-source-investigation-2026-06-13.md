# Cloud Hypervisor Virtiofs Snapshot Source Investigation

Date: 2026-06-13

Scope: local source investigation of whether Cloud Hypervisor snapshot/restore
can work with virtiofs/vhost-user-fs devices attached. Production runtime files
were not edited.

Local sources:

- `/Users/justin/code/cloud-hypervisor`
- `/Users/justin/code/virtiofsd`
- `/Users/justin/code/microvm.nix`

## Answer

Cloud Hypervisor snapshot/restore can work with built-in `--fs`
vhost-user-fs devices, but restore requires a usable virtiofsd backend on the
same socket path before Cloud Hypervisor reconstructs the VM.

The upstream Cloud Hypervisor integration test for snapshot/restore with
virtio-fs explicitly kills the old virtiofsd and starts a fresh virtiofsd on the
same socket path before running `cloud-hypervisor --restore`.

Relevant local source:

- `/Users/justin/code/cloud-hypervisor/cloud-hypervisor/tests/integration.rs:8931`
  defines `test_snapshot_restore_virtio_fs`.
- `/Users/justin/code/cloud-hypervisor/cloud-hypervisor/tests/integration.rs:9000`
  kills the old virtiofsd.
- `/Users/justin/code/cloud-hypervisor/cloud-hypervisor/tests/integration.rs:9004`
  starts a fresh virtiofsd reusing the same socket path.
- `/Users/justin/code/cloud-hypervisor/cloud-hypervisor/tests/integration.rs:9010`
  starts the restored VM.
- `/Users/justin/code/cloud-hypervisor/cloud-hypervisor/tests/integration.rs:9052`
  verifies virtiofs still works after restore.

This means the feature is not just theoretical.

## Why The Demo Likely Hung

Our demo kept the original `virtiofsd-run` process around while stopping the
source VM, then launched Cloud Hypervisor with `--restore`. The result was:

- no-share control: restore completed in `136ms`
- virtiofs case: snapshot completed in `649ms`, but restore API readiness timed
  out

Artifact:

- `reports/profile-artifacts/2026-06-13-cloud-hypervisor/mom-2-cloud-hypervisor-no-share-snapshot.tsv`
- `reports/profile-artifacts/2026-06-13-cloud-hypervisor/mom-2-cloud-hypervisor-virtiofs-snapshot.tsv`

The likely cause is that restore blocks while creating/restoring the vhost-user
fs device, before the API is ready. Cloud Hypervisor reconnects to the socket
while constructing the device from snapshot state, but the virtiofsd instance
from the source VM is the wrong lifecycle object for restore.

Source evidence:

- `/Users/justin/code/cloud-hypervisor/vmm/src/device_manager.rs:3271`
  creates the virtio-fs device with `Fs::new(...)`.
- `/Users/justin/code/cloud-hypervisor/vmm/src/device_manager.rs:3283`
  passes per-device snapshot state into `Fs::new(...)`.
- `/Users/justin/code/cloud-hypervisor/virtio-devices/src/vhost_user/fs.rs:90`
  connects to the vhost-user socket during `Fs::new`.
- `/Users/justin/code/cloud-hypervisor/virtio-devices/src/vhost_user/fs.rs:102`
  enters the restore branch when state is provided.
- `/Users/justin/code/cloud-hypervisor/virtio-devices/src/vhost_user/fs.rs:110`
  restores backend state through the vhost-user connection.

virtiofsd itself exits after its client disconnects:

- `/Users/justin/code/virtiofsd/src/main.rs:908` starts the vhost-user daemon.
- `/Users/justin/code/virtiofsd/src/main.rs:915` waits for the daemon.
- `/Users/justin/code/virtiofsd/src/main.rs:917` treats client disconnect as
  shutdown.

The Cloud Hypervisor live-migration test documents the expected behavior:

- `/Users/justin/code/cloud-hypervisor/cloud-hypervisor/tests/integration.rs:7300`
  starts a thread to wait for old virtiofsd to exit and start a replacement.
- `/Users/justin/code/cloud-hypervisor/cloud-hypervisor/tests/integration.rs:7302`
  says the source saves `DEVICE_STATE` then disconnects, causing virtiofsd to
  exit.
- `/Users/justin/code/cloud-hypervisor/cloud-hypervisor/tests/integration.rs:7303`
  says the destination needs a fresh virtiofsd to load `DEVICE_STATE`.
- `/Users/justin/code/cloud-hypervisor/cloud-hypervisor/tests/integration.rs:7359`
  removes the socket before migration so the destination cannot connect to the
  old virtiofsd.

That matches our symptom. The restore process likely connected poorly or waited
in the vhost-user-fs restore path while the old daemon/socket lifecycle was not
in the state Cloud Hypervisor expects.

## Device State Support

Cloud Hypervisor's built-in `--fs` device has explicit snapshot/migration code.

Source evidence:

- `/Users/justin/code/cloud-hypervisor/virtio-devices/src/vhost_user/fs.rs:342`
  implements `Snapshottable` for `Fs`.
- `/Users/justin/code/cloud-hypervisor/virtio-devices/src/vhost_user/fs.rs:347`
  snapshots via common vhost-user state.
- `/Users/justin/code/cloud-hypervisor/virtio-devices/src/vhost_user/fs.rs:353`
  implements `Migratable` for `Fs`.
- `/Users/justin/code/cloud-hypervisor/virtio-devices/src/vhost_user/mod.rs:747`
  captures common vhost-user state.
- `/Users/justin/code/cloud-hypervisor/virtio-devices/src/vhost_user/mod.rs:762`
  saves backend state if the backend supports device state.
- `/Users/justin/code/cloud-hypervisor/virtio-devices/src/vhost_user/vu_common_ctrl.rs:527`
  saves backend state via `SET_DEVICE_STATE_FD`.
- `/Users/justin/code/cloud-hypervisor/virtio-devices/src/vhost_user/vu_common_ctrl.rs:570`
  restores backend state.

virtiofsd supports this protocol:

- `/Users/justin/code/virtiofsd/doc/migration.md:4` documents migration through
  vhost-user's device-state interface.
- `/Users/justin/code/virtiofsd/doc/migration.md:35` says virtiofsd state can be
  stored and restored in a snapshot stream.
- `/Users/justin/code/virtiofsd/doc/migration.md:39` warns that file data itself
  is not included; the shared directory must match snapshot-time contents.
- `/Users/justin/code/virtiofsd/README.md:399` documents
  `--migration-mode=<find-paths|file-handles>`.

## microvm.nix Fit

microvm.nix uses Cloud Hypervisor's built-in `--fs`, not generic vhost-user:

- `/Users/justin/code/microvm.nix/lib/runners/cloud-hypervisor.nix:60`
  detects virtiofs shares.
- `/Users/justin/code/microvm.nix/lib/runners/cloud-hypervisor.nix:72`
  enables shared memory when virtiofs is used.
- `/Users/justin/code/microvm.nix/lib/runners/cloud-hypervisor.nix:239`
  emits `--fs tag=...,socket=...`.
- `/Users/justin/code/microvm.nix/nixos-modules/microvm/virtiofsd/default.nix:15`
  generates `virtiofsd-run`.
- `/Users/justin/code/microvm.nix/nixos-modules/microvm/virtiofsd/default.nix:45`
  runs virtiofsd with the configured socket and shared directory.

This is the right Cloud Hypervisor path. The generic vhost-user docs have a
snapshot limitation, but that applies to `--generic-vhost-user`, not the built-in
`--fs` path microvm.nix uses.

## Next Demo Fix

Update the demo restore sequence to match upstream:

1. Snapshot the paused VM.
2. Stop the source Cloud Hypervisor.
3. Stop the original `virtiofsd-run`.
4. Remove the old virtiofs socket file if it still exists.
5. Start a fresh `virtiofsd-run` with the same share source and same socket path.
6. Wait for the socket.
7. Start `cloud-hypervisor --restore source_url=...`.
8. Wait for API, resume, and verify `/workspace`.

For Agent Mom, this means Cloud Hypervisor remains a plausible single-runtime
answer, but restore orchestration must include a virtiofsd lifecycle. That is
still more declarative than a custom block-volume abstraction, but it is not
zero lifecycle work.
