#!/bin/sh
# Install FUSE-T and a fuse.pc that the `fuser` build script accepts (it asks pkg-config
# for `fuse >= 2.6.0`, while FUSE-T ships fuse-t.pc with a 1.x version).
set -eu
brew tap macos-fuse-t/homebrew-cask
brew install --cask fuse-t
brew install pkg-config
printf '%s\n' 'libdir=/usr/local/lib' 'Name: fuse' 'Description: FUSE-T as libfuse 2' \
  'Version: 2.9.9' 'Libs: -L${libdir} -Wl,-rpath,${libdir} -lfuse-t' \
  'Cflags: -I/usr/local/include/fuse' | sudo tee /usr/local/lib/pkgconfig/fuse.pc >/dev/null
