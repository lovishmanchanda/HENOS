#!/bin/sh
# Builds the Rust core for phones and puts it where the Android build looks
# for it. Needs the Android NDK (ANDROID_NDK_HOME) and cargo-ndk.
set -e
cd "$(dirname "$0")/../.."
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -P 26 \
  -o apps/android/app/src/main/jniLibs build -p henos-ffi --release
# Only our own library belongs in the app.
find apps/android/app/src/main/jniLibs -name "libiroh*.so" -delete
