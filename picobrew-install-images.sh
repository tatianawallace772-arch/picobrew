#!/usr/bin/env bash
#
# picobrew-install-images.sh — one-shot installer for picobrew.
#
# Builds picobrew from source, installs the binary, prepares a Homebrew-compatible
# prefix, warms the index cache, and (optionally) proves the install by pouring a
# real bottle. Non-interactive by design, so it also works inside container image
# builds and CI.
#
#   curl -fsSL https://raw.githubusercontent.com/tatianawallace772-arch/picobrew/main/picobrew-install-images.sh | bash
#   ./picobrew-install-images.sh                 # build from this checkout, install
#   ./picobrew-install-images.sh --verify        # ...and prove it: install + run jq
#   ./picobrew-install-images.sh --prefix ~/hb   # custom prefix
#   ./picobrew-install-images.sh --dry-run       # show the plan, touch nothing
#   ./picobrew-install-images.sh --src DIR       # use an existing checkout
#   ./picobrew-install-images.sh --from-git URL  # clone source to a temporary dir
#   ./picobrew-install-images.sh --bin-dir DIR   # choose where binary is installed
#   ./picobrew-install-images.sh --no-index     # skip index download
#   ./picobrew-install-images.sh --jobs N        # set cargo build parallelism
#
# Exit codes: 0 ok, 1 failure, 2 bad usage, 3 unsupported platform/toolchain.
#
# Requires: bash, curl, tar, and (for --from-git) git. Rust is bootstrapped
# automatically via rustup when no suitable toolchain is present.

set -euo pipefail

# ---------------------------------------------------------------- presentation

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  C_RESET=$'\033[0m'; C_DIM=$'\033[2m'; C_GREEN=$'\033[32m'
  C_YELLOW=$'\033[33m'; C_RED=$'\033[31m'; C_CYAN=$'\033[36m'; C_BOLD=$'\033[1m'
else
  C_RESET=''; C_DIM=''; C_GREEN=''; C_YELLOW=''; C_RED=''; C_CYAN=''; C_BOLD=''
fi

step()  { printf '%s==>%s %s\n' "$C_CYAN" "$C_RESET" "$*"; }
ok()    { printf '  %s✓%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
info()  { printf '  %s·%s %s\n' "$C_DIM" "$C_RESET" "$*"; }
warn()  { printf '  %s!%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
die()   { printf '%serror:%s %s\n' "$C_RED" "$C_RESET" "$1" >&2; exit "${2:-1}"; }

usage() {
  cat <<'USAGE'
Install picobrew from this checkout or a Git repository.
Usage: bash picobrew-install-images.sh [options]
  --prefix DIR     Installation prefix
  --bin-dir DIR    Directory for the picobrew executable
  --src DIR        Build from an existing checkout
  --from-git URL   Clone a source repository to a temporary directory
  --no-index       Skip warming the formula index
  --verify         Install a jq bottle and verify it runs
  --dry-run        Show the plan without making changes
  -j, --jobs N     Number of Cargo build jobs
  -q, --quiet      Reduce installer messages
  -h, --help       Show this help
When piped into bash, the public picobrew repository is cloned by default.
USAGE
}

# ---------------------------------------------------------------------- config

PREFIX_OPT=''
BIN_DIR_OPT=''
SRC_DIR=''
GIT_URL=''
JOBS=''
DO_INDEX=1
DO_VERIFY=0
DRY_RUN=0
QUIET=0

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix)      PREFIX_OPT="${2:?--prefix needs a directory}"; shift 2 ;;
    --prefix=*)    PREFIX_OPT="${1#*=}"; [ -n "$PREFIX_OPT" ] || die "--prefix needs a directory" 2; shift ;;
    --bin-dir)     BIN_DIR_OPT="${2:?--bin-dir needs a directory}"; shift 2 ;;
    --bin-dir=*)   BIN_DIR_OPT="${1#*=}"; [ -n "$BIN_DIR_OPT" ] || die "--bin-dir needs a directory" 2; shift ;;
    --src)         SRC_DIR="${2:?--src needs a directory}"; shift 2 ;;
    --src=*)       SRC_DIR="${1#*=}"; [ -n "$SRC_DIR" ] || die "--src needs a directory" 2; shift ;;
    --from-git)    GIT_URL="${2:?--from-git needs a URL}"; shift 2 ;;
    --from-git=*)  GIT_URL="${1#*=}"; [ -n "$GIT_URL" ] || die "--from-git needs a URL" 2; shift ;;
    -j|--jobs)     JOBS="${2:?--jobs needs a number}"; shift 2 ;;
    --jobs=*)      JOBS="${1#*=}"; [ -n "$JOBS" ] || die "--jobs needs a number" 2; shift ;;
    --no-index)    DO_INDEX=0; shift ;;
    --verify)      DO_VERIFY=1; shift ;;
    --dry-run)     DRY_RUN=1; shift ;;
    -q|--quiet)    QUIET=1; shift ;;
    -h|--help)     usage; exit 0 ;;
    *)             die "unknown argument: $1 (try --help)" 2 ;;
  esac
done

# HOME is needed for Rust and picobrew state. Fail clearly instead of relying
# on an unbound or empty HOME and accidentally constructing paths under /.
if [ -z "${HOME:-}" ]; then
  die "HOME is unset or empty; set HOME to your home directory before running the installer" 2
fi
if [ ! -d "$HOME" ]; then
  die "HOME is not an existing directory: $HOME" 2
fi
PATH=${PATH:-/usr/local/bin:/usr/bin:/bin}
export PATH

# A script read from stdin has no source file; clone instead of treating the
# caller's working directory as a checkout.
SCRIPT_FILE=${BASH_SOURCE[0]:-}
SCRIPT_DIR=''
if [ -n "$SCRIPT_FILE" ] && [ -f "$SCRIPT_FILE" ]; then
  SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$SCRIPT_FILE")" && pwd)
fi
DEFAULT_GIT_URL='https://github.com/tatianawallace772-arch/picobrew.git'
if [ -z "$GIT_URL" ] && [ -z "$SRC_DIR" ] && { [ -z "$SCRIPT_DIR" ] || [ ! -f "$SCRIPT_DIR/Cargo.toml" ]; }; then
  GIT_URL=$DEFAULT_GIT_URL
fi

run() { # run <cmd...> — echo under --dry-run, execute otherwise
  if [ "$DRY_RUN" = 1 ]; then
    printf '  %s$ %s%s\n' "$C_DIM" "$*" "$C_RESET"
  else
    "$@"
  fi
}

T0=$SECONDS
TMPDIR_PB=''
cleanup() { if [ -n "${TMPDIR_PB:-}" ]; then rm -rf "$TMPDIR_PB"; fi; }
trap cleanup EXIT

elapsed() { echo "$(( SECONDS - T0 ))s"; }
human_bytes() { # bytes -> "3.4 MiB"
  awk -v b="${1:-0}" 'BEGIN{ split("B KiB MiB GiB",u," "); i=1;
    while (b>=1024 && i<4) { b/=1024; i++ }
    printf (i==1 ? "%d %s" : "%.1f %s"), b, u[i] }'
}

# ------------------------------------------------------------------ preflight

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1" 3
}

step "picobrew installer"
[ "$QUIET" = 1 ] || info "dry run: $([ "$DRY_RUN" = 1 ] && echo yes || echo no)"

OS=$(uname -s 2>/dev/null || echo unknown)
ARCH=$(uname -m 2>/dev/null || echo unknown)
case "$OS" in
  Linux|Darwin) ;;
  *) die "unsupported OS: $OS (picobrew targets Linux and macOS)" 3 ;;
esac
case "$ARCH" in
  x86_64|amd64)  ARCH=x86_64 ;;
  arm64|aarch64) ARCH=aarch64 ;;
  *) warn "untested architecture: $ARCH — continuing, but bottles may not exist for it" ;;
esac
ok "$OS $ARCH"

need_cmd curl
need_cmd tar

# -------------------------------------------------------------------- prefix

if [ -n "$PREFIX_OPT" ]; then
  PREFIX=$PREFIX_OPT
elif [ -n "${PICOBREW_PREFIX:-}" ]; then
  PREFIX=$PICOBREW_PREFIX
  info "prefix from PICOBREW_PREFIX"
else
  if [ "$OS" = Darwin ]; then
    if [ "$ARCH" = aarch64 ]; then PREFIX=/opt/homebrew; else PREFIX=/usr/local; fi
  else
    PREFIX=/home/linuxbrew/.linuxbrew
  fi
fi

# Normalise: expand a leading ~ and make absolute.
case "$PREFIX" in "~") PREFIX=$HOME ;; "~/"*) PREFIX="$HOME/${PREFIX#\~/}" ;; esac
case "$PREFIX" in /*) ;; *) PREFIX="$PWD/$PREFIX" ;; esac
if [ "$PREFIX" != / ]; then PREFIX=${PREFIX%/}; fi

# writable_or_creatable <dir>
writable_or_creatable() {
  local d=$1 p
  if [ -d "$d" ]; then [ -w "$d" ]; return; fi
  p=$(dirname -- "$d")
  while [ ! -d "$p" ] && [ "$p" != "/" ]; do p=$(dirname -- "$p"); done
  [ -w "$p" ]
}

IS_ROOT=0; [ "$(id -u)" = 0 ] && IS_ROOT=1

if ! writable_or_creatable "$PREFIX"; then
  if [ "$IS_ROOT" = 0 ]; then
    if [ -n "$PREFIX_OPT" ] || [ -n "${PICOBREW_PREFIX:-}" ]; then
      die "requested prefix is not writable: $PREFIX" 1
    fi
    FALLBACK="$HOME/.pb"
    warn "default prefix $PREFIX is not writable by $(id -un)"
    warn "falling back to $FALLBACK; set PICOBREW_PREFIX to use it later"
    PREFIX=$FALLBACK
  fi
fi

# Homebrew bottles hard-code the build cellared path; relocation must fit in the
# original slot. Very long prefixes can make some ELF patches impossible.
PREFIX_BYTES=$(LC_ALL=C printf '%s' "$PREFIX" | wc -c | tr -d ' ')
if [ "$OS" = Linux ] && [ "$PREFIX_BYTES" -gt 19 ]; then
  warn "prefix is $PREFIX_BYTES bytes; some Linux bottles require a prefix of at most 19 bytes"
  warn "those bottles will fail to install; consider a shorter --prefix"
fi

BIN_DIR=${BIN_DIR_OPT:-}
if [ -z "$BIN_DIR" ]; then
  if writable_or_creatable "$PREFIX/bin"; then BIN_DIR="$PREFIX/bin"; else BIN_DIR="$HOME/.local/bin"; fi
fi
case "$BIN_DIR" in "~") BIN_DIR=$HOME ;; "~/"*) BIN_DIR="$HOME/${BIN_DIR#\~/}" ;; esac
case "$BIN_DIR" in /*) ;; *) BIN_DIR="$PWD/$BIN_DIR" ;; esac

ok "prefix: $PREFIX"
ok "binary: $BIN_DIR/picobrew"

# --------------------------------------------------------------------- source

if [ -n "$GIT_URL" ] && [ -n "$SRC_DIR" ]; then
  die "--src and --from-git cannot be combined" 2
fi
if [ -n "$GIT_URL" ]; then
  need_cmd git
  if [ "$DRY_RUN" = 1 ]; then
    SRC="<temporary checkout>"
    info "would clone $GIT_URL into a temporary directory"
  else
    TMPDIR_PB=$(mktemp -d)
    SRC="$TMPDIR_PB/source"
    step "source: clone $GIT_URL"
    git clone --depth 1 -- "$GIT_URL" "$SRC" || die "git clone failed"
  fi
elif [ -n "$SRC_DIR" ]; then
  SRC=$(CDPATH= cd -- "$SRC_DIR" && pwd) || die "--src directory not found: $SRC_DIR" 2
else
  SRC=$SCRIPT_DIR
fi
if [ "$DRY_RUN" != 1 ] || [ -z "$GIT_URL" ]; then
  [ -f "$SRC/Cargo.toml" ] || die "no Cargo.toml in $SRC — the repository must contain Cargo.toml and src/ at its root, or pass --src" 2
fi
ok "source: $SRC"

# ------------------------------------------------------------------ toolchain

cargo_ok=0
if command -v cargo >/dev/null 2>&1; then
  have=$(rustc --version 2>/dev/null | awk '{print $2}') || have=''
  if printf '%s\n' "$have" | awk -F. '{ exit !($1 ~ /^[0-9]+$/ && $2 ~ /^[0-9]+$/ && ($1 > 1 || ($1 == 1 && $2 >= 89))) }'; then
    cargo_ok=1
  else
    warn "rustc ${have:-unknown} is older than the required 1.89"
  fi
fi

if [ "$cargo_ok" = 0 ]; then
  step "toolchain: installing Rust (rustup, minimal profile)"
  if [ "$DRY_RUN" = 1 ]; then
    info "would download https://sh.rustup.rs and install stable with --no-modify-path"
  else
    export RUSTUP_HOME="${PICOBREW_RUSTUP_HOME:-$HOME/.rustup}"
    export CARGO_HOME="${PICOBREW_CARGO_HOME:-$HOME/.cargo}"
    if [ -z "$TMPDIR_PB" ]; then TMPDIR_PB=$(mktemp -d); fi
    curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 \
      https://sh.rustup.rs -o "$TMPDIR_PB/rustup-init.sh" \
      || die "could not download rustup-init"
    head -c 400 "$TMPDIR_PB/rustup-init.sh" | grep -qi 'rust\|#!/bin/sh' \
      || die "rustup-init.sh does not look like a shell script; refusing to run it"
    sh "$TMPDIR_PB/rustup-init.sh" -y --no-modify-path --profile minimal \
      --default-toolchain stable \
      || die "rustup install failed (see output above)"
    PATH="$CARGO_HOME/bin:$PATH"; export PATH
    ok "rustc $(rustc --version | awk '{print $2}') at $CARGO_HOME"
  fi
else
  ok "rustc $(rustc --version | awk '{print $2}')"
fi

# ---------------------------------------------------------------------- build

step "build: cargo build --release"
BUILD_ARGS=(build --release --manifest-path "$SRC/Cargo.toml")
[ -f "$SRC/Cargo.lock" ] && BUILD_ARGS+=(--locked)
case "$JOBS" in
  '') ;;
  *[!0-9]*|0) die "--jobs must be a positive integer" 2 ;;
  *) BUILD_ARGS+=(--jobs "$JOBS") ;;
esac
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$SRC/target}"

if [ "$DRY_RUN" = 1 ]; then
  run cargo "${BUILD_ARGS[@]}"
else
  build_start=$SECONDS
  cargo "${BUILD_ARGS[@]}" || die "cargo build failed (rerun with the output above to see why)"
  [[ "$QUIET" = 1 ]] || ok "built in $(( SECONDS - build_start ))s"
fi

BINARY="$CARGO_TARGET_DIR/release/picobrew"
if [ "$DRY_RUN" = 0 ]; then
  [ -x "$BINARY" ] || die "build finished but $BINARY is missing"
  ok "binary $(human_bytes "$(wc -c < "$BINARY" | tr -d ' ')")"
fi

# -------------------------------------------------------------------- install

step "install: $BIN_DIR/picobrew"
run mkdir -p "$BIN_DIR"
run install -m 755 "$BINARY" "$BIN_DIR/picobrew"

if [ "$DRY_RUN" = 0 ]; then
  got=$("$BIN_DIR/picobrew" --version 2>/dev/null || true)
  [ -n "$got" ] || die "installed binary does not run: $BIN_DIR/picobrew"
  ok "$got"

  # A second picobrew earlier in PATH would silently shadow this one.
  if resolved=$(command -v picobrew 2>/dev/null) && [ -n "$resolved" ]; then
    resolved=$(readlink -f "$resolved" 2>/dev/null || echo "$resolved")
    if [ "$resolved" != "$(readlink -f "$BIN_DIR/picobrew")" ]; then
      warn "another picobrew is earlier in PATH: $resolved"
      warn "export PATH=\"$BIN_DIR:\$PATH\" to use this install"
    fi
  fi
fi

# --------------------------------------------------------------------- prefix

step "prefix: preparing $PREFIX"
run mkdir -p "$PREFIX/Cellar" "$PREFIX/opt"
if [ "$DRY_RUN" = 0 ]; then
  for d in Cellar opt; do
    [ -w "$PREFIX/$d" ] || die "$PREFIX/$d is not writable by $(id -un)"
  done
  ok "Cellar and opt ready"
fi

# ---------------------------------------------------------------------- index

if [ "$DO_INDEX" = 1 ]; then
  step "index: warming the formula cache"
  if [ "$DRY_RUN" = 1 ]; then
    run "$BIN_DIR/picobrew" --prefix "$PREFIX" index --refresh
  else
    idx_start=$SECONDS
    if "$BIN_DIR/picobrew" --prefix "$PREFIX" index --refresh >/dev/null 2>&1; then
      ok "index refreshed in $(( SECONDS - idx_start ))s"
    else
      warn "index refresh failed (offline?) — picobrew will retry on first use"
    fi
  fi
else
  info "skipping index download (--no-index)"
fi

# --------------------------------------------------------------------- verify

if [ "$DO_VERIFY" = 1 ]; then
  step "verify: pour a real bottle (jq)"
  if [ "$DRY_RUN" = 1 ]; then
    run "$BIN_DIR/picobrew" --prefix "$PREFIX" install jq
    info "would then run: $PREFIX/bin/jq --version"
  else
    if "$BIN_DIR/picobrew" --prefix "$PREFIX" install jq; then
      jq_ver=$(printf '{"a":[1,2,3]}\n' | "$PREFIX/bin/jq" -c '.a | add' 2>/dev/null || true)
      if [ "$jq_ver" = "6" ]; then
        ok "real bottle installed, relocated, and executing"
      else
        die "jq installed but verification failed (output: ${jq_ver:-nothing}); run $PREFIX/bin/jq to inspect the error"
      fi
    else
      die "verification install failed — the picobrew binary remains installed"
    fi
  fi
fi

# ------------------------------------------------------------------- doctor

if [ "$DRY_RUN" = 0 ]; then
  step "doctor"
  "$BIN_DIR/picobrew" --prefix "$PREFIX" doctor || warn "doctor reported problems (see above)"
fi

# ------------------------------------------------------------------ summary

echo
if [ "$DRY_RUN" = 1 ]; then
  printf '%s%s would be installed%s\n' "$C_BOLD" 'picobrew' "$C_RESET"
else
  printf '%s%s installed%s\n' "$C_BOLD" 'picobrew' "$C_RESET"
fi
info "binary   $BIN_DIR/picobrew"
info "prefix   $PREFIX"
info "cache    ${PICOBREW_HOME:-$HOME/.picobrew}"
info "elapsed  $(elapsed)"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo
     echo "Add picobrew to PATH:"
     echo "  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

if [ "$PREFIX" != /opt/homebrew ] && [ "$PREFIX" != /usr/local ] && [ "$PREFIX" != /home/linuxbrew/.linuxbrew ]; then
  echo
  echo "picobrew is using a non-default prefix. Persist it:"
  echo "  export PICOBREW_PREFIX=\"$PREFIX\""
fi

if [ ! -t 1 ]; then
  # Likely a container image build — emit runnable ENV lines instead of prose.
  echo
  echo "Container image:"
  echo "  ENV PATH=\"$BIN_DIR:\$PATH\""
  [ "$PREFIX" != /home/linuxbrew/.linuxbrew ] && echo "  ENV PICOBREW_PREFIX=\"$PREFIX\""
fi

if [ "$DRY_RUN" = 0 ]; then
  echo
  echo "Next:"
  echo "  picobrew search ripgrep"
  echo "  picobrew install ripgrep"
fi
