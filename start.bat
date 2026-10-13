@echo off
rem SnapClip - run locally without Docker
cd /d "%~dp0"
if exist "rust\target\release\snapclip.exe" (
  "rust\target\release\snapclip.exe"
) else (
  echo First run: building the backend (one time only)...
  cd rust && cargo build --release && cd ..
  "rust\target\release\snapclip.exe"
)
