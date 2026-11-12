@echo off
rem One-time setup on Windows: fetches Gradle and creates this project's
rem Gradle wrapper (gradlew). Everything stays inside the repository folder.
setlocal
cd /d "%~dp0"
if "%JAVA_HOME%"=="" set "JAVA_HOME=%ProgramFiles%\Android\Android Studio\jbr"
set "GRADLE_USER_HOME=%~dp0..\..\.gradle-home"
set "DIST=%~dp0..\..\.gradle-dist"
if not exist "%DIST%" mkdir "%DIST%"
if not exist "%DIST%\gradle-9.9.1\bin\gradle.bat" (
  curl.exe -sSfL -o "%DIST%\gradle.zip" https://services.gradle.org/distributions/gradle-9.9.1-bin.zip || exit /b 1
  tar -xf "%DIST%\gradle.zip" -C "%DIST%" || exit /b 1
  del "%DIST%\gradle.zip"
)
call "%DIST%\gradle-9.9.1\bin\gradle.bat" wrapper --gradle-version 9.9.1 --no-validate-url -q
