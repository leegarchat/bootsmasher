#!/usr/bin/env bash
# bootsmasher static multi-arch builder (modeled on lgz_compress/build.sh).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# llvm-mingw (mstorsjo) provides the only native aarch64 Windows linker
# (aarch64-w64-mingw32-clang). It is referenced by absolute path on the
# winarm64 step only: its bin/ dir also shadows distro gcc names with
# clang shims, so it must NOT leak into the global PATH.
LLVM_MINGW_CLANG="/opt/llvm-mingw/bin/aarch64-w64-mingw32-clang"

DIST_DIR="$SCRIPT_DIR/dist"
BIN_NAME="bootsmasher"

LINUX_X64="x86_64-unknown-linux-musl"
LINUX_X86="i686-unknown-linux-musl"
LINUX_ARM64="aarch64-unknown-linux-musl"
LINUX_ARM32="armv7-unknown-linux-musleabihf"
WIN_X64="x86_64-pc-windows-gnu"
# NOTE: aarch64-pc-windows-gnu does not exist in rustc; the ARM64 Windows
# target is aarch64-pc-windows-gnullvm (Clang/LLVM ABI, mingw libs).
WIN_ARM64="aarch64-pc-windows-gnullvm"

METHOD="auto"      # auto | cargo | cross
SELECTED_ARCH="all" # all | x64 | x86 | arm64 | arm32 | win64 | winarm64 | linux | windows | small

usage() {
    cat <<EOF
Usage:
  $0 [OPTIONS]

Build method options:
  --cargo          Force local cargo (needs musl/cross gcc, mingw for windows)
  --cross          Force cross (needs Docker or Podman + cross)
  --auto           Auto-select: cross if containers exist, else cargo (default)

Architecture selection:
  --arch <type>    all (default: full set + small set), linux (4 musl),
                   windows (2 gnu), small (6 size-first vboot+install),
                   small-x64, small-x86, small-arm64, small-arm32,
                   small-win64, small-winarm64 (one small binary each),
                   x64, x86, arm64, arm32, win64, winarm64
  -h, --help       Show this message

Examples:
  $0 --cargo --arch x64
  $0 --arch windows
  $0 --arch small
  $0 --arch all

Outputs:
  dist/bootsmasher-linux-*    Static musl binaries
  dist/bootsmasher-windows-*.exe
  dist/bootsmasher-small-*    Size-first builds (profile small,
                              --no-default-features --features small:
                              vboot + install only, for recovery)
EOF
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --cargo) METHOD="cargo"; shift ;;
        --cross) METHOD="cross"; shift ;;
        --auto) METHOD="auto"; shift ;;
        --arch)
            SELECTED_ARCH="${2:-}"
            [[ -z "$SELECTED_ARCH" ]] && { echo "Error: --arch needs a value"; exit 1; }
            shift 2 ;;
        -h|--help) usage ;;
        *) echo "Unknown parameter: $1"; usage ;;
    esac
done

detect_pkg_manager() {
    if command -v apt-get &>/dev/null; then echo "apt";
    elif command -v dnf &>/dev/null; then echo "dnf";
    elif command -v pacman &>/dev/null; then echo "pacman";
    else echo "unknown"; fi
}

suggest_install() {
    echo ""
    echo "Missing build dependencies for '$1'."
    case "$(detect_pkg_manager)" in
        apt) echo "  sudo apt update && sudo apt install -y musl-tools gcc-i686-linux-gnu gcc-aarch64-linux-gnu gcc-arm-linux-gnueabihf mingw-w64" ;;
        dnf) echo "  sudo dnf install -y musl-gcc gcc-i686-linux-gnu gcc-aarch64-linux-gnu mingw64-gcc" ;;
        pacman) echo "  sudo pacman -S --needed musl aarch64-linux-gnu-gcc arm-linux-gnueabihf-gcc mingw-w64-gcc" ;;
        *) echo "  Install musl + cross gcc + mingw-w64 with your package manager." ;;
    esac
    echo "Windows gnu targets also need: rustup target add $WIN_X64 $WIN_ARM64"
    echo "Windows ARM64 links via llvm-mingw (mstorsjo), not distro mingw:"
    echo "  curl -LO https://github.com/mstorsjo/llvm-mingw/releases/download/20240619/llvm-mingw-20240619-ucrt-ubuntu-20.04-x86_64.tar.xz"
    echo "  sudo tar -xf llvm-mingw-20240619-ucrt-ubuntu-20.04-x86_64.tar.xz -C /opt/"
    echo "  sudo mv /opt/llvm-mingw-20240619-ucrt-ubuntu-20.04-x86_64 /opt/llvm-mingw"
    echo "(build.sh picks up /opt/llvm-mingw/bin automatically.)"
    echo ""
}

has_container_engine() {
    command -v podman &>/dev/null || (command -v docker &>/dev/null && docker info &>/dev/null)
}

check_linker() {
    case "$1" in
        "$LINUX_X86") command -v i686-linux-gnu-gcc &>/dev/null ;;
        "$LINUX_ARM64") command -v aarch64-linux-gnu-gcc &>/dev/null ;;
        "$LINUX_ARM32") command -v arm-linux-gnueabihf-gcc &>/dev/null ;;
        "$LINUX_X64") command -v musl-gcc &>/dev/null || return 0 ;;
        "$WIN_X64") command -v x86_64-w64-mingw32-gcc &>/dev/null && [[ "$(command -v x86_64-w64-mingw32-gcc)" != /opt/llvm-mingw/* ]] || [[ -x /usr/bin/x86_64-w64-mingw32-gcc ]] ;;
        "$WIN_ARM64") [[ -x "$LLVM_MINGW_CLANG" ]] ;;
    esac
}

BUILDER=""
if [[ "$METHOD" == "cross" ]]; then
    command -v cross &>/dev/null && has_container_engine || { suggest_install "cross"; exit 1; }
    BUILDER="cross"
elif [[ "$METHOD" == "cargo" ]]; then
    BUILDER="cargo"
else
    if command -v cross &>/dev/null && has_container_engine; then BUILDER="cross"; else BUILDER="cargo"; fi
fi
echo "==> Build mode: $BUILDER"

if [[ "$BUILDER" == "cargo" ]]; then
    rustup target add "$LINUX_X64" "$LINUX_X86" "$LINUX_ARM64" "$LINUX_ARM32" "$WIN_X64" "$WIN_ARM64" >/dev/null 2>&1 || true
fi

setup_linkers() {
    # cross drives its own linkers via container images, except
    # winarm64: cross has no image for it and falls back to host
    # cargo, which needs the llvm-mingw linker from the environment.
    if [[ "$BUILDER" != "cargo" && "$1" != "$WIN_ARM64" ]]; then return 0; fi
    case "$1" in
        "$LINUX_ARM64") export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="aarch64-linux-gnu-gcc" ;;
        "$LINUX_ARM32") export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER="arm-linux-gnueabihf-gcc" ;;
        "$LINUX_X86") export CARGO_TARGET_I686_UNKNOWN_LINUX_MUSL_LINKER="i686-linux-gnu-gcc" ;;
        "$LINUX_X64") command -v musl-gcc &>/dev/null && export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="musl-gcc" || true ;;
        "$WIN_X64")
            # Absolute distro path: /opt/llvm-mingw shadows the gcc name
            # with a clang shim when it leaks into PATH.
            if [[ -x /usr/bin/x86_64-w64-mingw32-gcc ]]; then
                export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="/usr/bin/x86_64-w64-mingw32-gcc"
            else
                export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="x86_64-w64-mingw32-gcc"
            fi
            ;;
        "$WIN_ARM64") export CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER="$LLVM_MINGW_CLANG" ;;	    esac
}

build_target() {
    local target="$1" output_name="$2" small="${3:-0}"
    echo ""
    echo "------------------------------------------------------------"
    if [[ "$small" == 1 ]]; then
        echo "Building [small $output_name] -> $target"
    else
        echo "Building [$output_name] -> $target"
    fi
    echo "------------------------------------------------------------"
    if [[ "$BUILDER" == "cargo" ]] && ! check_linker "$target"; then
        suggest_install "$target"
        echo "Skipping $target (no linker)."
        return 0
    fi
    setup_linkers "$target"
    local ext=""; [[ "$target" == *windows* ]] && ext=".exe"
    local src_bin out_name
    if [[ "$small" == 1 ]]; then
        if [[ "$BUILDER" == "cross" ]]; then
            cross build --profile small --no-default-features --features small --target "$target"
        else
            cargo build --profile small --no-default-features --features small --target "$target"
        fi
        src_bin="$SCRIPT_DIR/target/$target/small/${BIN_NAME}${ext}"
        out_name="${BIN_NAME}-small-${output_name}${ext}"
    else
        if [[ "$BUILDER" == "cross" ]]; then
            cross build --release --target "$target"
        else
            cargo build --release --target "$target"
        fi
        src_bin="$SCRIPT_DIR/target/$target/release/${BIN_NAME}${ext}"
        out_name="${BIN_NAME}-${output_name}${ext}"
    fi
    if [[ -f "$src_bin" ]]; then
        cp "$src_bin" "$DIST_DIR/${out_name}"
        local size
        size=$(stat -c%s "$DIST_DIR/${out_name}" 2>/dev/null || stat -f%z "$DIST_DIR/${out_name}")
        echo "Success: $DIST_DIR/${out_name} ($size bytes)"
    else
        echo "Error: binary not found: $src_bin"
        exit 1
    fi
}

rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

build_linux() {
    build_target "$LINUX_X64" "linux-x86_64"
    build_target "$LINUX_X86" "linux-x86"
    build_target "$LINUX_ARM64" "linux-arm64"
    build_target "$LINUX_ARM32" "linux-arm32"
}

build_windows() {
    build_target "$WIN_X64" "windows-x86_64"
    build_target "$WIN_ARM64" "windows-arm64"
}

# Size-first set (vboot+install only, for recovery ramdisks).
build_small() {
    build_target "$LINUX_X64" "linux-x86_64" 1
    build_target "$LINUX_X86" "linux-x86" 1
    build_target "$LINUX_ARM64" "linux-arm64" 1
    build_target "$LINUX_ARM32" "linux-arm32" 1
    build_target "$WIN_X64" "windows-x86_64" 1
    build_target "$WIN_ARM64" "windows-arm64" 1
}

case "$SELECTED_ARCH" in
    all) build_linux; build_windows; build_small ;;
    linux) build_linux ;;
    windows) build_windows ;;
    small) build_small ;;
    x64) build_target "$LINUX_X64" "linux-x86_64" ;;
    x86) build_target "$LINUX_X86" "linux-x86" ;;
    arm64) build_target "$LINUX_ARM64" "linux-arm64" ;;
    arm32) build_target "$LINUX_ARM32" "linux-arm32" ;;
    win64) build_target "$WIN_X64" "windows-x86_64" ;;
    winarm64) build_target "$WIN_ARM64" "windows-arm64" ;;
    small-x64) build_target "$LINUX_X64" "linux-x86_64" 1 ;;
    small-x86) build_target "$LINUX_X86" "linux-x86" 1 ;;
    small-arm64) build_target "$LINUX_ARM64" "linux-arm64" 1 ;;
    small-arm32) build_target "$LINUX_ARM32" "linux-arm32" 1 ;;
    small-win64) build_target "$WIN_X64" "windows-x86_64" 1 ;;
    small-winarm64) build_target "$WIN_ARM64" "windows-arm64" 1 ;;
    *) echo "Error: unknown arch '$SELECTED_ARCH'"; usage ;;
esac

echo ""
echo "============================================================"
echo "Done! Binaries in dist/:"
ls -lh "$DIST_DIR"
echo "============================================================"
