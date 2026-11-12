@echo off
rem Builds the Rust core for phones and puts it where the Android build
rem looks for it. Run from apps\android on Windows. Needs the Android NDK
rem (ANDROID_NDK_HOME) and cargo-ndk.
setlocal
if "%ANDROID_NDK_HOME%"=="" for /d %%d in ("%LOCALAPPDATA%\Android\Sdk\ndk\*") do set ANDROID_NDK_HOME=%%d
cd /d "%~dp0..\.."
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -P 26 -o apps\android\app\src\main\jniLibs build -p henos-ffi --release
rem Only our own library belongs in the app.
for /r "apps\android\app\src\main\jniLibs" %%f in (libiroh*.so) do del "%%f"
