#!/usr/bin/env bash
# Run the Linux-only parts of the test suite in a container.
#
# The FUSE adapter and Landlock confinement cannot be exercised on macOS, and shipping either
# untested would break the property every other milestone here holds. This makes that runnable
# from a Darwin host rather than depending on someone having a Linux box.
#
# Usage:
#   scripts/linux-test.sh                      # everything Linux-only
#   scripts/linux-test.sh -p surrealfs-sandbox # one package
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE=surrealfs-linux-test

# The workspace depends on a SurrealDB checkout outside the repository, so the container needs it
# mounted at the identical absolute path for the path dependency to resolve.
SURREALDB_PATH="$(awk -F'"' '/^surrealdb = \{ path =/ {print $2}' "$REPO/Cargo.toml")"
SURREALDB_ROOT="$(dirname "$SURREALDB_PATH")"

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "building $IMAGE..."
    docker build -f "$REPO/docker/linux-test.Dockerfile" -t "$IMAGE" "$REPO"
fi

# The minimal privileges that actually work, established by testing each in turn:
#   --device /dev/fuse            the FUSE character device
#   --cap-add SYS_ADMIN           mount(2)
#   --security-opt apparmor=...   AppArmor is what blocks the mount, not seccomp
# Full --privileged is deliberately not used: a harness that needs it teaches the wrong
# deployment story, and it turned out not to be necessary.
#
# Landlock needs none of the above — it is unprivileged by design.
exec docker run --rm -it \
    -v "$REPO":/work \
    -v "$SURREALDB_ROOT":"$SURREALDB_ROOT":ro \
    -w /work \
    --device /dev/fuse \
    --cap-add SYS_ADMIN \
    --security-opt apparmor=unconfined \
    -e CARGO_PROFILE_DEV_DEBUG=0 \
    -e CARGO_PROFILE_TEST_DEBUG=0 \
    "$IMAGE" \
    cargo test "${@:--p surrealfs-fuse -p surrealfs-sandbox}"
