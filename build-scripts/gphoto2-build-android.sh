#!/usr/bin/env bash
#
# gphoto2-build-android.sh [arm64|x64|all] — libgphoto2 2.5.34 + libusb 1.0.30 + libltdl 2.5.4 per Android (M8.1),
# cross-compilati con l'NDK 30 (android-31), pagine da 16 KB. Piano e disposizione nell'APK: docs/ANDROID-GPHOTO2.md.
#   build-thirdparty/jniLibs/<abi>/  libltdl.so  libusb-1.0.so  libgphoto2_port.so  libgphoto2.so
#                                    libgphoto2_camlib_ptp2.so  libgphoto2_iolib_ptpip.so
#                                    libgphoto2_iolib_prism_usbhost.so   (l'iolib di Prism sull'fd di UsbManager)
#   build-thirdparty/licenses/       LICENSE-libgphoto2.txt  LICENSE-libgphoto2_port.txt  LICENSE-libusb.txt
#                                    LICENSE-libltdl.txt  libgphoto2-patches/*.patch  BUILD-INFO-gphoto2-<abi>.txt
# LGPL-2.1+: SOLO .so separate, mai linkate statiche in libprism.so (CLAUDE.md § Regole). Il gemello di
# third_party/libgphoto2/build-libgphoto2.ps1 di prism-win@09a79893 (gli stessi tarball, le stesse 5 patch del Mac, lo
# stesso configure: camlib ptp2 sola; niente libexif, libxml, curl, gd, jpeg, seriale, lockdev) con due differenze:
# l'iolib usb1 NON si costruisce (al suo posto prism_usbhost, core/android/tether/iolib/: Android non lascia enumerare
# /dev/bus/usb) e libgphoto2_port non vede libusb (--with-libusb-1.0=no): libusb la usa solo prism_usbhost.
# I nomi sono quelli che il packager dell'APK estrae (lib*.so, senza versione: il libtool dei tarball conosce
# *-linux-android e non mette versioni); libltdl trova i moduli con le cartelle di collegamenti di
# core/android/tether/gphoto2_modules.cpp (ptp2.so → libgphoto2_camlib_ptp2.so, …).
# Variabili: ANDROID_HOME, NDK_VERSION (30.0.16248370), ANDROID_API (31), JOBS, FORCE=1 ricostruisce comunque.
# --with-vusb (M8.2, SOLO PROVE): in più l'iolib «vusb» del tarball (libgphoto2_port/vusb: una fotocamera PTP VIRTUALE,
# un Nikon D750 finto che serve i file di $VCAMERADIR e «scatta» duplicando un JPEG; nessuna fotocamera vera) in
# build-thirdparty/gphoto2-vusb/<abi>/libgphoto2_iolib_vusb.so, MAI in jniLibs: la build normale non la contiene. La
# copia a mano chi fa una build di prova (docs/ANDROID-TETHER-UI.md §3), e la barra la dichiara «SIMULATED CAMERA».
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
NDK_VERSION="${NDK_VERSION:-30.0.16248370}"
NDK="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/$NDK_VERSION}"
API="${ANDROID_API:-31}"
TC="${NDK}/toolchains/llvm/prebuilt/darwin-x86_64/bin"
OUT="${ROOT}/build-thirdparty"
W="${OUT}/gphoto2"
DL="${W}/downloads"
PATCHES="${ROOT}/third_party/libgphoto2/patches"
IOLIB_SRC="${ROOT}/core/android/tether/iolib"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || echo 4)}"
[[ -x "${TC}/clang" ]] || { echo "NDK non trovato in ${NDK}" >&2; exit 1; }

# I tarball ufficiali, inchiodati per sha256: gli hash sono quelli del donatore (build-libgphoto2.ps1, = Scripts/
# build-libgphoto2.sh del Mac) e, per libgphoto2 e libusb, coincidono col «digest» che GitHub pubblica per l'asset
# della release (api.github.com/repos/<repo>/releases/tags/<tag>, controllato il 20/11/2026). libtool: ftp.gnu.org
# (l'hash è quello del donatore; GNU pubblica solo la firma .sig accanto al tarball).
PINS=(
  "libgphoto2-2.5.34.tar.bz2 https://github.com/gphoto/libgphoto2/releases/download/v2.5.34/libgphoto2-2.5.34.tar.bz2 5df9e774eb4ab6087a193afff84dcafe9f666e3443fe29e73e6779f659ff7f28"
  "libusb-1.0.30.tar.bz2 https://github.com/libusb/libusb/releases/download/v1.0.30/libusb-1.0.30.tar.bz2 fea36f34f9156400209595e300840767ab1a385ede1dc7ee893015aea9c6dbaf"
  "libtool-2.5.4.tar.xz https://ftp.gnu.org/gnu/libtool/libtool-2.5.4.tar.xz f81f5860666b0bc7d84baddefa60d1cb9fa6fceb2398cc3baca6afaa60266675"
)

fetch() {
  mkdir -p "${DL}"
  local p name url sha got
  for p in "${PINS[@]}"; do
    read -r name url sha <<<"${p}"
    if [[ ! -f "${DL}/${name}" ]]; then
      echo "== scarico ${name}"
      curl -fsSL --retry 3 -o "${DL}/${name}.part" "${url}"
      mv "${DL}/${name}.part" "${DL}/${name}"
    fi
    got="$(shasum -a 256 "${DL}/${name}" | cut -c1-64)"
    if [[ "${got}" != "${sha}" ]]; then
      rm -f "${DL}/${name}"
      echo "!! ${name}: sha256 ${got}, atteso ${sha} (file tolto)" >&2; exit 1
    fi
    echo "   sha256 ok  ${name}"
  done
}

key() {  # tarball + patch + iolib di Prism + questo script + piattaforma: se non cambia, le .so in cache restano
  { shasum -a 256 "${DL}"/*.tar.* | cut -c1-64
    (cd "${ROOT}" && find third_party/libgphoto2/patches core/android/tether/iolib -type f ! -name '.DS_Store' -print0 \
       | LC_ALL=C sort -z | xargs -0 shasum -a 256)
    shasum -a 256 < "$0"; echo "abi=$1 api=${API} ndk=${NDK_VERSION}"
  } | shasum -a 256 | cut -c1-64
}

# run <log> <comando…>: il comando con l'uscita nel log; se fallisce, l'errore esatto (la coda del log) e stop.
run() {
  local log="$1"; shift
  if ! "$@" >>"${log}" 2>&1; then
    echo "!! fallito: $* (log: ${log})" >&2; tail -40 "${log}" >&2; exit 1
  fi
}

# build_vusb <abi> <triple>: l'iolib vusb contro l'albero già configurato da build_abi (config.h, header interni).
build_vusb() {
  local abi="$1" triple="$2" P="${W}/$1/build/libgphoto2-2.5.34/libgphoto2_port" V="${OUT}/gphoto2-vusb/$1"
  [[ -f "${P}/config.h" ]] || { echo "!! ${abi}: manca l'albero di libgphoto2 (${P}): FORCE=1 per ricostruirlo" >&2; exit 1; }
  mkdir -p "${V}"
  run "${V}/vusb.log" "${TC}/${triple}${API}-clang" -shared -fPIC -O2 -std=gnu11 -DHAVE_CONFIG_H -D_GPHOTO2_INTERNAL_CODE \
    -DVCAMERADIR=\"/data/local/tmp/vcamera\" -I"${P}" -I"${P}/libgphoto2_port" "${P}/vusb/vusb.c" "${P}/vusb/vcamera.c" \
    -o "${V}/libgphoto2_iolib_vusb.so" -Wl,-soname,vusb.so -Wl,-z,max-page-size=16384 -Wl,--build-id=sha1 -Wl,-z,defs \
    -L"${W}/${abi}/stage/lib" -lgphoto2_port
  "${TC}/llvm-strip" --strip-unneeded "${V}/libgphoto2_iolib_vusb.so"
  echo "   [${abi}] vusb (SOLO PROVE, camera simulata): ${V}/libgphoto2_iolib_vusb.so — non in jniLibs"
}

build_abi() {
  local abi="$1" triple="$2"
  local B="${W}/${abi}/build" ST="${W}/${abi}/stage" L="${W}/${abi}/logs"
  rm -rf "${B}" "${ST}" "${L}"; mkdir -p "${B}" "${ST}" "${L}"
  export CC="${TC}/${triple}${API}-clang" CXX="${TC}/${triple}${API}-clang++"
  export AR="${TC}/llvm-ar" RANLIB="${TC}/llvm-ranlib" NM="${TC}/llvm-nm" STRIP="${TC}/llvm-strip"
  export OBJDUMP="${TC}/llvm-objdump" LD="${TC}/ld.lld"
  export CFLAGS="-O2 -fPIC -fvisibility=default" CPPFLAGS="-I${ST}/include"
  export LDFLAGS="-L${ST}/lib -Wl,-z,max-page-size=16384 -Wl,--build-id=sha1"
  export PKG_CONFIG_LIBDIR="${ST}/lib/pkgconfig" PKG_CONFIG_PATH=""
  local host=(--host="${triple}" --build="$(uname -m | sed 's/arm64/aarch64/')-apple-darwin" --prefix="${ST}"
              --disable-static --enable-shared)
  cd "${B}"
  tar xf "${DL}/libtool-2.5.4.tar.xz"; tar xf "${DL}/libusb-1.0.30.tar.bz2"; tar xf "${DL}/libgphoto2-2.5.34.tar.bz2"

  echo "   [${abi}] libltdl 2.5.4"
  (cd libtool-2.5.4/libltdl && run "${L}/libltdl.log" ./configure "${host[@]}" --enable-ltdl-install \
     && run "${L}/libltdl.log" make -j"${JOBS}" && run "${L}/libltdl.log" make install)

  echo "   [${abi}] libusb 1.0.30 (senza udev: su Android niente enumerazione, solo libusb_wrap_sys_device)"
  (cd libusb-1.0.30 && run "${L}/libusb.log" ./configure "${host[@]}" --disable-udev --disable-examples-build \
     --disable-tests-build && run "${L}/libusb.log" make -j"${JOBS}" && run "${L}/libusb.log" make install)

  echo "   [${abi}] libgphoto2 2.5.34 + le 5 patch di Prism (camlib ptp2; iolib ptpip)"
  cd libgphoto2-2.5.34
  local p
  for p in "${PATCHES}"/libgphoto2-2.5.34-*.patch; do run "${L}/patch.log" patch -p1 --forward -i "${p}"; done
  run "${L}/libgphoto2.configure.log" env LTDLINCL="-I${ST}/include" LIBLTDL="-L${ST}/lib -lltdl" \
    ac_cv_func_malloc_0_nonnull=yes ac_cv_func_realloc_0_nonnull=yes \
    ./configure "${host[@]}" --disable-nls --disable-rpath \
      --with-camlibs=ptp2 --without-libexif --with-libxml-2.0=no --with-libcurl=no --with-gdlib=no --without-jpeg \
      --disable-serial --disable-lockdev --disable-ttylock --disable-disk --with-libusb-1.0=no --with-libusb=no
  run "${L}/libgphoto2.make.log" make -j"${JOBS}"
  run "${L}/libgphoto2.make.log" make install
  cd "${B}"

  echo "   [${abi}] l'iolib prism_usbhost (core/android/tether/iolib)"
  # Contro gli header del tarball: gphoto2-port-library.h non è installato (come prism_wpd / prism_icc).
  run "${L}/prism_usbhost.log" "${CC}" -shared -fPIC -O2 -std=c11 -Wall -Wextra -Werror \
    -I"${ST}/include" -I"${ST}/include/libusb-1.0" -I"${B}/libgphoto2-2.5.34/libgphoto2_port" \
    "${IOLIB_SRC}/prism_usbhost.c" "${IOLIB_SRC}/prism_usbhost_io.c" \
    -o "${B}/libgphoto2_iolib_prism_usbhost.so" -Wl,-soname,libgphoto2_iolib_prism_usbhost.so \
    -Wl,-z,max-page-size=16384 -Wl,--build-id=sha1 -Wl,-z,defs -L"${ST}/lib" -lgphoto2_port -lusb-1.0

  # Il pacchetto: i nomi dell'APK. I moduli di libtool hanno SONAME «ptp2.so» / «ptpip.so»: nessuno li mette in
  # NEEDED (li apre libltdl per percorso), quindi il nome del file può cambiare senza rifare il link.
  local J="${OUT}/jniLibs/${abi}" camdir iodir
  camdir="$(ls -d "${ST}"/lib/libgphoto2/2.5.34)"; iodir="$(ls -d "${ST}"/lib/libgphoto2_port/0.12.*)"
  mkdir -p "${J}"
  rm -f "${J}"/libltdl.so "${J}"/libusb-1.0.so "${J}"/libgphoto2*.so
  cp "${ST}/lib/libltdl.so" "${ST}/lib/libusb-1.0.so" "${ST}/lib/libgphoto2_port.so" "${ST}/lib/libgphoto2.so" "${J}/"
  cp "${camdir}/ptp2.so" "${J}/libgphoto2_camlib_ptp2.so"
  cp "${iodir}/ptpip.so" "${J}/libgphoto2_iolib_ptpip.so"
  cp "${B}/libgphoto2_iolib_prism_usbhost.so" "${J}/"
  local f
  for f in "${J}"/libltdl.so "${J}"/libusb-1.0.so "${J}"/libgphoto2*.so; do "${STRIP}" --strip-unneeded "${f}"; done

  local LIC="${OUT}/licenses"
  mkdir -p "${LIC}/libgphoto2-patches"
  cp "${B}/libgphoto2-2.5.34/COPYING" "${LIC}/LICENSE-libgphoto2.txt"
  cp "${B}/libgphoto2-2.5.34/libgphoto2_port/COPYING.LIB" "${LIC}/LICENSE-libgphoto2_port.txt"
  cp "${B}/libusb-1.0.30/COPYING" "${LIC}/LICENSE-libusb.txt"
  cp "${B}/libtool-2.5.4/libltdl/COPYING.LIB" "${LIC}/LICENSE-libltdl.txt"
  cp "${PATCHES}"/*.patch "${LIC}/libgphoto2-patches/"
  { echo "libgphoto2 2.5.34 (camlib ptp2; iolib ptpip + prism_usbhost) per Prism Android ${abi}, NDK ${NDK_VERSION}, android-${API}"
    for p in "${PINS[@]}"; do read -r name _ sha <<<"${p}"; echo "${name} sha256 ${sha}"; done
    echo "patch:"; (cd "${PATCHES}" && shasum -a 256 ./*.patch)
    echo "iolib prism_usbhost:"; (cd "${IOLIB_SRC}" && shasum -a 256 ./*)
    echo ".so:"; (cd "${J}" && shasum -a 256 libltdl.so libusb-1.0.so libgphoto2*.so)
  } > "${LIC}/BUILD-INFO-gphoto2-${abi}.txt"
}

report() {  # misura, allineamento a 16 KB (ogni LOAD a 0x4000, obbligo Play da Android 15), SONAME e NEEDED
  local abi="$1" J="${OUT}/jniLibs/$1" f bad
  for f in "${J}"/libltdl.so "${J}"/libusb-1.0.so "${J}"/libgphoto2*.so; do
    bad="$("${TC}/llvm-readelf" -lW "${f}" | awk '$1=="LOAD" && $NF!="0x4000"')"
    [[ -z "${bad}" ]] || { echo "!! ${f}: LOAD non allineati a 16 KB:" >&2; echo "${bad}" >&2; exit 1; }
    printf '   %-44s %8s byte  LOAD 0x4000 x%s  SONAME %s  NEEDED %s\n' "${abi}/$(basename "${f}")" \
      "$(stat -f %z "${f}")" "$("${TC}/llvm-readelf" -lW "${f}" | grep -c ' LOAD ')" \
      "$("${TC}/llvm-readelf" -d "${f}" | sed -n 's/.*(SONAME).*\[\(.*\)\]/\1/p')" \
      "$("${TC}/llvm-readelf" -d "${f}" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | paste -sd, -)"
    if "${TC}/llvm-readelf" -d "${f}" | grep -E '\((NEEDED|SONAME)\)' | grep -qE '\.so\.[0-9]'; then
      echo "!! ${f}: un nome con la versione (l'APK porta solo lib*.so)" >&2; exit 1
    fi
  done
}

VUSB=0
if [[ "${1:-}" == "--with-vusb" ]]; then VUSB=1; shift; fi
case "${1:-all}" in
  arm64) ABIS=(arm64-v8a) ;; x64) ABIS=(x86_64) ;; all) ABIS=(arm64-v8a x86_64) ;;
  *) echo "uso: $0 [--with-vusb] [arm64|x64|all]" >&2; exit 2 ;;
esac
t0=${SECONDS}
fetch
for abi in "${ABIS[@]}"; do
  case "${abi}" in arm64-v8a) triple=aarch64-linux-android ;; x86_64) triple=x86_64-linux-android ;; esac
  k="$(key "${abi}")"; stamp="${W}/${abi}/.stamp"
  if [[ -z "${FORCE:-}" && -f "${stamp}" && "$(cat "${stamp}")" == "${k}" && -f "${OUT}/jniLibs/${abi}/libgphoto2.so" ]]; then
    echo "== ${abi}: in cache (chiave ${k:0:12})"
  else
    echo "== ${abi}: costruisco (android-${API}, chiave ${k:0:12})"
    (build_abi "${abi}" "${triple}")
    echo "${k}" > "${stamp}"
  fi
  report "${abi}"
  if [[ "${VUSB}" == 1 ]]; then build_vusb "${abi}" "${triple}"; fi
done
echo "gphoto2-build-android: fatto in $((SECONDS - t0)) s"
