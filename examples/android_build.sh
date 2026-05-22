#/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"/..

usage() {
    (
        { set +x; } 2>/dev/null

        cat <<-EOF
			Usage:
			    # Optional
			    export ANDROID_SERIAL="...";

			    CARGO_PKG="..." \\
			    EXAMPLE="..." \\
			    FEATURES="..." \\
			    "${0}"

			Examples:
			    CARGO_PKG="dial9-perf-self-profile" \\
			    EXAMPLE="basic" \\
			    FEATURES="" \\
			    "${0}"
			or:
			    CARGO_PKG="dial9-tokio-telemetry" \\
			    EXAMPLE="cpu_profile_workload" \\
			    FEATURES="-Fcpu-profiling" \\
			    "${0}"

		EOF

        false
    ) >&2
}

set -x

pwd

ANDROID_SERIAL="${ANDROID_SERIAL:-}" # Optional, to ensure `adb` commands are not ambiguous
CARGO_PKG="${CARGO_PKG:?missing. $(usage)}"
EXAMPLE="${EXAMPLE:?missing. $(usage)}"
PROFILE="${PROFILE:-release-with-debug}"
FEATURES=${FEATURES:-} # e.g., -Fcpu-profiling

TARGET="aarch64-linux-android" # Hard-coded

NDK_HOME="${NDK_HOME:?}"  # used to locate cross-compilation helpers

export AR="$(find $NDK_HOME -name "llvm-ar" -type f | head -n1)"
export RANLIB="$(find $NDK_HOME -name "llvm-ranlib" -type f | head -n1)"

export CC_aarch64_linux_android="$(find $NDK_HOME -name "aarch64-linux-android*-clang" -type f | head -n1)"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="${CC_aarch64_linux_android}"

# Cargo build

# Tested with cargo 1.95.0-nightly (85eff7c80 2026-01-15)
CARGO_BUILD_CMD=(
    cargo build
        # The profile; `release` for speed, but `-with-debug` info attached
        --profile "${PROFILE}"
        # The package; ~ `Cargo.toml` file/dir
        -p "${CARGO_PKG}"
        # its features to be enabling
        ${FEATURES}
        # The specific `examples/${EXAMPLE}.rs` file to be targeting
        --example "${EXAMPLE}"
        # We are cross-compiling to `aarch64-linux-android`
        --target "${TARGET}"
        # This may require recompiling the stdlib
        -Zbuild-std
); "${CARGO_BUILD_CMD[@]}"

# ADB

adb push \
    "target/$TARGET/$PROFILE/examples/$EXAMPLE" \
    /data/local/tmp/ \
;

adb shell "cd /data/local/tmp && './$EXAMPLE'; echo Files:; ls \$PWD/*; echo" >&2

{ set +x; } 2>/dev/null

cat <<-'EOF' >&2
	Done.

	You may open a shell at the desired location by doing:

	    adb shell
	    # In the so-obtained shell:
	    cd /data/local/tmp/

	You may also pull the files from there by running:

	    adb pull \
	        /data/local/tmp/cpu_profile_trace.0.bin.gz \
	        ~/Downloads/cpu_profile_trace_"$(date +"%Y-%m-%d_%H-%M-%S")".bin.gz \
	    ;
EOF
