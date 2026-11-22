# Prism for Android — source code of the LGPL libraries

Prism for Android (`com.rf.node.image.prism`, published by Rainbow Factory) ships these libraries as **separate
shared libraries**, loaded at run time and replaceable. This repository is the complete corresponding source for the
LGPL parts of Prism for Android **26.6 (versionCode 1)**, and it stays available for at least three years after the
last distribution of that version.

| Library in the app | Licence | Source here |
|---|---|---|
| `libgphoto2.so`, `libgphoto2_camlib_ptp2.so`, `libgphoto2_iolib_ptpip.so` | LGPL-2.1-or-later | `upstream/libgphoto2-2.5.34.tar.bz2` + `patches/libgphoto2/*.patch` |
| `libgphoto2_port.so` | LGPL-2.0-or-later | same archive |
| `libgphoto2_iolib_prism_usbhost.so` | LGPL-2.1-or-later | `prism-iolib/` (a libgphoto2_port I/O module written for Prism: a USB port over the file descriptor Android's UsbManager gives the app) |
| `libusb-1.0.so` | LGPL-2.1-or-later | `upstream/libusb-1.0.30.tar.bz2` |
| `libltdl.so` | LGPL-2.1-or-later | `upstream/libtool-2.5.4.tar.xz` (libltdl) |
| `librawler_ffi.so` | LGPL-2.1 | `upstream/rawler-0.8.0.crate` (rawler from crates.io) + `rawler-ffi/` (the C interface built on it; `Cargo.lock` pins every Rust dependency, all from crates.io) |

The upstream archives are byte-identical to the official releases (`SHA256SUMS`; URLs below). Nothing else of Prism
is linked into these libraries; Prism's own code in `libprism.so` uses them only through their public interfaces
(rawler through `dlopen`).

- libgphoto2 2.5.34 — https://github.com/gphoto/libgphoto2/releases/download/v2.5.34/libgphoto2-2.5.34.tar.bz2
- libusb 1.0.30 — https://github.com/libusb/libusb/releases/download/v1.0.30/libusb-1.0.30.tar.bz2
- libtool 2.5.4 — https://ftp.gnu.org/gnu/libtool/libtool-2.5.4.tar.xz
- rawler 0.8.0 — https://crates.io/crates/rawler/0.8.0

## Build

`build-scripts/` holds the two scripts used for the release (comments in Italian): `gphoto2-build-android.sh`
(Android NDK 30.0.16248370, API 31, arm64-v8a and x86_64, 16 KB pages; configure options, the five patches applied
in order, the `prism_usbhost` module) and `rawler-build-android.sh` (`cargo ndk`, same NDK and API, `cdylib`). In the
Prism tree the scripts expect the archives and patches at `third_party/` and the I/O module at
`core/android/tether/iolib/`; adjust those paths to this layout.

To use a modified library, replace the corresponding `.so` inside the app's native library folder: Prism loads them
by name at run time.

## Written offer

On request we also send this source on a physical medium for no more than the cost of performing the distribution.
Write to **support@rainbowfactory.uk**. This offer is valid for at least three years from the last time we
distribute Prism for Android 26.6.
