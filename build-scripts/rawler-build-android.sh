#!/usr/bin/env bash
#
# rawler-build-android.sh [arm64-v8a|x86_64…] — librawler_ffi.so (third_party/rawler-ffi: la ABI C sopra rawler 0.8,
# il ripiego RAW del motore, D9 di Windows) per Android, con cargo-ndk.
#   build-thirdparty/jniLibs/<abi>/librawler_ffi.so     arm64-v8a, x86_64; android-31; pagine da 16 KB
#   build-thirdparty/licenses/LICENSE-rawler.txt        la licenza accanto (LGPL-2.1)
# LGPL-2.1: una .so SEPARATA, mai linkata statica nel core (CLAUDE.md § Regole). Il Cargo.toml copiato da Windows
# è già `crate-type = ["cdylib"]`: nessuna patch. `cargo --locked` (il Cargo.lock è la pin, 135 pacchetti); prima
# --offline (la cache di Cargo), poi in rete da crates.io. Il cargo target dir sta in build-thirdparty/ (gitignorato).
# Da flare-android scripts/lightcraft-raw-build.sh (stessi RUSTFLAGS: max-page-size=16384 + soname).
# Variabili: ANDROID_HOME, NDK_VERSION (30.0.16248370), ANDROID_API (31), FORCE=1 ricompila comunque.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
NDK_VERSION="${NDK_VERSION:-30.0.16248370}"
NDK="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/$NDK_VERSION}"
API="${ANDROID_API:-31}"
OUT="${ROOT}/build-thirdparty"
SRC="${ROOT}/third_party/rawler-ffi"
LIB=librawler_ffi.so
# cargo: quello di rustup (in ~/.cargo/bin c'è solo cargo-ndk su questo Mac), poi la toolchain stable.
TC="${HOME}/.rustup/toolchains/stable-aarch64-apple-darwin/bin"
if command -v rustup >/dev/null 2>&1 && C="$(rustup which cargo 2>/dev/null)"; then TC="$(dirname "${C}")"; fi
export PATH="${TC}:${HOME}/.cargo/bin:${PATH}"
command -v cargo >/dev/null || { echo "cargo non trovato (rustup toolchain install stable)" >&2; exit 1; }
cargo ndk --version >/dev/null 2>&1 || { echo "cargo-ndk non trovato (cargo install cargo-ndk)" >&2; exit 1; }
[[ -f "${NDK}/build/cmake/android.toolchain.cmake" ]] || { echo "NDK non trovato in ${NDK}" >&2; exit 1; }
READELF="${NDK}/toolchains/llvm/prebuilt/darwin-x86_64/bin/llvm-readelf"
ABIS=("$@"); [[ ${#ABIS[@]} -eq 0 ]] && ABIS=(arm64-v8a x86_64)

key() {  # sorgenti + script + piattaforma + cargo: se non cambia, la .so in cache resta
  { (cd "${ROOT}" && find third_party/rawler-ffi -type f ! -name '.DS_Store' -print0 | LC_ALL=C sort -z | xargs -0 shasum -a 256)
    shasum -a 256 < "$0"; echo "abi=$1 api=${API} ndk=${NDK_VERSION}"; cargo --version
  } | shasum -a 256 | cut -c1-64
}

for abi in "${ABIS[@]}"; do
  case "${abi}" in
    arm64-v8a) triple=aarch64-linux-android ;;
    x86_64)    triple=x86_64-linux-android ;;
    *) echo "ABI sconosciuta: ${abi}" >&2; exit 2 ;;
  esac
  dir="${OUT}/rawler/${abi}"; td="${OUT}/rawler/cargo"; k="$(key "${abi}")"
  dst="${OUT}/jniLibs/${abi}/${LIB}"
  if [[ -z "${FORCE:-}" && -f "${dst}" && -f "${dir}/.stamp" && "$(cat "${dir}/.stamp")" == "${k}" ]]; then
    echo "== ${abi}: ${LIB} in cache (chiave ${k:0:12})"
  else
    echo "== ${abi}: compilo ${LIB} (android-${API}, chiave ${k:0:12})"
    mkdir -p "${dir}" "$(dirname "${dst}")"
    run=(env ANDROID_NDK_HOME="${NDK}"
         RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=-Wl,-soname,${LIB}"
         cargo ndk -t "${abi}" --platform "${API}" build --release --locked --target-dir "${td}")
    log="${dir}/cargo.log"
    if (cd "${SRC}" && "${run[@]}" --offline >"${log}" 2>&1); then echo "   (offline, dalla cache di Cargo)"
    elif (cd "${SRC}" && "${run[@]}" >>"${log}" 2>&1); then echo "   (con la rete: crates.io)"
    else echo "!! cargo ${LIB} (${abi}) fallito: ${log}" >&2; tail -30 "${log}" >&2; exit 1; fi
    cp "${td}/${triple}/release/${LIB}" "${dst}"
    echo "${k}" > "${dir}/.stamp"
  fi
  # Pagine da 16 KB: ogni LOAD allineato a 0x4000 (obbligo Play per Android 15+).
  bad="$("${READELF}" -lW "${dst}" | awk '$1=="LOAD" && $NF!="0x4000"')"
  [[ -z "${bad}" ]] || { echo "!! ${dst}: LOAD non allineati a 16 KB:" >&2; echo "${bad}" >&2; exit 1; }
  printf '   %-28s %10s byte  sha256 %s  LOAD 0x4000 x%s\n' "${abi}/${LIB}" "$(stat -f %z "${dst}")" \
    "$(shasum -a 256 "${dst}" | cut -c1-16)" "$("${READELF}" -lW "${dst}" | grep -c ' LOAD ')"
done
mkdir -p "${OUT}/licenses"
cp "${SRC}/LICENSE-rawler.txt" "${OUT}/licenses/LICENSE-rawler.txt"
