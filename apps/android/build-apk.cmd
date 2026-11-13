@echo off
rem Builds the Android app on Windows (the debug build unless told otherwise). Run build-rust.cmd first.
setlocal
cd /d "%~dp0"
if "%JAVA_HOME%"=="" set "JAVA_HOME=%ProgramFiles%\Android\Android Studio\jbr"
set "GRADLE_USER_HOME=%~dp0..\..\.gradle-home"
if not exist local.properties (
  >local.properties echo sdk.dir=%LOCALAPPDATA:\=/%/Android/Sdk
)
rem Use the Gradle fetched by setup-gradle.cmd when it is there; otherwise the
rem wrapper downloads its own.
set "GRADLE=%~dp0..\..\.gradle-dist\gradle-9.9.1\bin\gradle.bat"
if not exist "%GRADLE%" set "GRADLE=gradlew.bat"
rem Pass a task to build something else, for example: build-apk.cmd assembleRelease
set "TASK=%~1"
if "%TASK%"=="" set "TASK=assembleDebug"
call "%GRADLE%" %TASK% --console=plain
