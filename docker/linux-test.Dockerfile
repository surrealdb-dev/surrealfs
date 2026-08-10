# Linux test environment for the parts that cannot be exercised on macOS.
#
# Two milestone items live behind this image: the FUSE adapter, which needs a real
# /dev/fuse and a kernel that will accept a mount, and Linux namespace confinement.
# Neither can be verified on a Darwin host, and shipping either untested would break
# the property that every other milestone in this repository holds.
#
# Run it with the minimal privileges that actually work, established empirically:
#
#   --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined
#
# AppArmor is the blocker for mount(2), not seccomp — SYS_ADMIN alone fails, and
# seccomp=unconfined alone fails. Full --privileged is not required and is not used,
# because a test harness that needs it teaches the wrong deployment story.
FROM rust:1-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        fuse3 \
        libfuse3-dev \
        pkg-config \
        kmod \
    && rm -rf /var/lib/apt/lists/*

# Let a non-root user mount, which is how an agent process would actually run.
RUN echo "user_allow_other" >> /etc/fuse.conf

WORKDIR /work

# The macOS and Linux builds must not share a target directory: same paths, different
# object formats, and cargo will happily rebuild the world back and forth.
ENV CARGO_TARGET_DIR=/work/target-linux
